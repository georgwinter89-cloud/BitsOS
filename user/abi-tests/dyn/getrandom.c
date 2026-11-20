/* dyn-getrandom (L7-SYNC): the vDSO's getrandom (Linux 7.3.1's lib/vdso/getrandom.c) as glibc uses it and
 * called directly: the params query, the bytes, every fallback to the syscall, and separate output across fork
 * and exec. "glibc fills without the syscall" is checked with a seccomp filter that makes the getrandom syscall
 * fail after glibc's state has its key. Modes: glibc (the params query and glibc's use; needs MAP_DROPPABLE
 * from mmap), direct (the function on a state of this program's own); both then exec. Prints yes/no relations and
 * errno names only. */
#define _GNU_SOURCE
#include <errno.h>
#include <linux/filter.h>
#include <linux/seccomp.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mman.h>
#include <sys/prctl.h>
#include <sys/random.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "vdsosym.h"

typedef long (*vgr_fn)(void *, size_t, unsigned, void *, size_t);

static void yes(const char *l, int c) {
  printf("%s: %s\n", l, c ? "yes" : "no");
}

static const char *en(long r) {
  switch (-r) {
  case EINVAL: return "-EINVAL";
  case EFAULT: return "-EFAULT";
  case ENOSYS: return "-ENOSYS";
  case EPERM: return "-EPERM";
  }
  static char b[24];
  snprintf(b, sizeof b, "%ld", r);
  return b;
}

static int nonzero(const unsigned char *b, size_t n) {
  for (size_t i = 0; i < n; i++)
    if (b[i]) return 1;
  return 0;
}

/* After this, the getrandom syscall answers EPERM in this process. */
static void deny_syscall(void) {
  struct sock_filter f[] = {
    BPF_STMT(BPF_LD | BPF_W | BPF_ABS, offsetof(struct seccomp_data, nr)),
    BPF_JUMP(BPF_JMP | BPF_JEQ | BPF_K, SYS_getrandom, 0, 1),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | EPERM),
    BPF_STMT(BPF_RET | BPF_K, SECCOMP_RET_ALLOW),
  };
  struct sock_fprog p = {4, f};
  prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
  if (prctl(PR_SET_SECCOMP, SECCOMP_MODE_FILTER, &p)) perror("seccomp");
}

/* The params query: glibc uses the vDSO only when it answers 0 (and then mmaps the states as it says). */
static void params(vgr_fn f) {
  yes("__vdso_getrandom", f != 0);
  if (!f) return;
  uint32_t params[16];
  memset(params, 0xff, sizeof params);
  long r = f(0, 0, 0, params, ~0ul);
  printf("params query: %s\n", en(r));
  if (r == 0) {
    printf("  state size %u, prot %#x, flags %#x\n", params[0], params[1], params[2]);
    int z = 1;
    for (int i = 3; i < 16; i++) z &= params[i] == 0;
    yes("  reserved words zero", z);
  }
}

static void via_glibc(void) {
  unsigned char b[32], c[32];
  yes("glibc getrandom", getrandom(b, 32, 0) == 32 && getrandom(c, 32, 0) == 32 && memcmp(b, c, 32) && nonzero(b, 32));
  int st;
  /* A fork child's glibc states are wiped (MAP_DROPPABLE): it fetches a new key first, so the filter is set in
   * a child that made one call before. */
  pid_t k = fork();
  if (k == 0) {
    getrandom(b, 16, 0);
    deny_syscall();
    int ok = 1;
    for (int i = 0; i < 20; i++) ok &= getrandom(b, 16, 0) == 16;
    _exit(ok ? 0 : 1);
  }
  waitpid(k, &st, 0);
  yes("glibc fills from the vDSO, no syscall", WIFEXITED(st) && WEXITSTATUS(st) == 0);

  int fd[2];
  if (pipe(fd)) return;
  getrandom(b, 32, 0);
  k = fork();
  if (k == 0) {
    getrandom(c, 32, 0);
    if (write(fd[1], c, 32) != 32) _exit(1);
    _exit(0);
  }
  getrandom(b, 32, 0);
  waitpid(k, &st, 0);
  if (read(fd[0], c, 32) != 32) memset(c, 0, 32);
  yes("after fork parent and child get different bytes", memcmp(b, c, 32) != 0);
  close(fd[0]);
  close(fd[1]);
}

static void direct(void) {
  vgr_fn f = (vgr_fn)vdso_sym((void *)getauxval(AT_SYSINFO_EHDR), "__vdso_getrandom", "LINUX_2.6");
  yes("__vdso_getrandom", f != 0);
  if (!f) return;
  /* A state of our own, wiped on fork as MAP_DROPPABLE memory is. */
  unsigned char *pg = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  madvise(pg, 4096, MADV_WIPEONFORK);
  unsigned char a[200], b[200];
  long r = f(a, 200, 0, pg, 144);
  long r2 = f(b, 200, 0, pg, 144);
  yes("200 bytes twice, different", r == 200 && r2 == 200 && memcmp(a, b, 200) && nonzero(a, 200));
  printf("length 0: %s\n", en(f(a, 0, 0, pg, 144)));
  printf("flags 8 (the syscall's answer): %s\n", en(f(a, 16, 8, pg, 144)));
  printf("state size 100 (the syscall): %s\n", en(f(a, 16, 0, pg, 100)));
  printf("state across a page edge: %s\n", en(f(a, 16, 0, pg + 4000, 144)));
  printf("GRND_NONBLOCK|GRND_INSECURE: %s\n", en(f(a, 16, 5, pg, 144)));
  int fd[2];
  if (pipe(fd)) return;
  pid_t k = fork();
  if (k == 0) {
    f(b, 64, 0, pg, 144);
    if (write(fd[1], b, 64) != 64) _exit(1);
    _exit(0);
  }
  f(a, 64, 0, pg, 144);
  int st;
  waitpid(k, &st, 0);
  if (read(fd[0], b, 64) != 64) memset(b, 0, 64);
  yes("state wiped on fork: different bytes in the child", memcmp(a, b, 64) != 0);
}

static double ns_now(void) {
  struct timespec t;
  clock_gettime(CLOCK_MONOTONIC, &t);
  return t.tv_sec * 1e9 + t.tv_nsec;
}

/* Not in cases.txt (prints times): 16-byte requests through the syscall, glibc and the vDSO on an own state. */
static void bench(void) {
  vgr_fn f = (vgr_fn)vdso_sym((void *)getauxval(AT_SYSINFO_EHDR), "__vdso_getrandom", "LINUX_2.6");
  unsigned char b[16], *pg = mmap(0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  const int n = 200000;
  for (int round = 0; round < 3; round++) {
    double t0 = ns_now();
    for (int i = 0; i < n; i++) syscall(SYS_getrandom, b, 16, 0);
    double t1 = ns_now();
    for (int i = 0; i < n; i++) getrandom(b, 16, 0);
    double t2 = ns_now();
    if (f)
      for (int i = 0; i < n; i++) f(b, 16, 0, pg, 144);
    double t3 = ns_now();
    printf("bench: syscall %.1f ns, glibc %.1f ns, vdso %.1f ns per 16 bytes\n", (t1 - t0) / n, (t2 - t1) / n, f ? (t3 - t2) / n : -1.0);
  }
}

int main(int argc, char **argv) {
  setvbuf(stdout, 0, _IONBF, 0);
  const char *m = argc > 1 ? argv[1] : "";
  if (!strcmp(m, "after-exec")) {
    unsigned char b[16];
    yes("after exec: getrandom", getrandom(b, 16, 0) == 16 && nonzero(b, 16));
    return 0;
  }
  if (!strcmp(m, "bench")) {
    bench();
    return 0;
  }
  if (!strcmp(m, "glibc")) {
    params((vgr_fn)vdso_sym((void *)getauxval(AT_SYSINFO_EHDR), "__vdso_getrandom", "LINUX_2.6"));
    via_glibc();
  } else if (!strcmp(m, "direct")) {
    direct();
  } else {
    puts("mode: glibc or direct");
    return 2;
  }
  fflush(stdout);
  execl("/proc/self/exe", argv[0], "after-exec", (char *)0);
  printf("exec: %s\n", strerror(errno));
  return 1;
}
