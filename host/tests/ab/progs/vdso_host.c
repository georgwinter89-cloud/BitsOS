/* AB-14: the V2 vDSO image (crates/proc's build output) run on this host. The image is mapped as the kernel maps
 * it (the vvar page right before it, the text read+exec) and the page is filled by hand with a made-up clock,
 * so every answer can be checked exactly: the TSC read before and after a call bounds what the vDSO may
 * answer (base_ns + (tsc - base_tsc) * mult >> 32, plus the offsets). Then the fallbacks (MODE_NONE, clocks the
 * vDSO does not serve, getcpu without F_GETCPU) against this host's syscalls, a TSC behind the base, and a
 * seqlock torture: a writer flips between two equal descriptions of one clock line 100 ms of TSC apart while
 * readers check every answer against the line (a torn read is 100 ms off).
 * Build: gcc -O2 -pthread -I<layout.h dir> -Iuser/abi-tests/c vdso_host.c; run: ./vdso_host <vdso.so>. */
#define _GNU_SOURCE
#include "layout.h"
#include "vdsosym.h"
#include <cpuid.h>
#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#include <x86intrin.h>

typedef int (*cgt_fn)(clockid_t, struct timespec *);
typedef int (*gtod_fn)(struct timeval *, void *);
typedef long (*time_fn)(long *);
typedef long (*getcpu_fn)(unsigned *, unsigned *, void *);
typedef int (*getres_fn)(clockid_t, struct timespec *);

static volatile unsigned char *vv;
static cgt_fn cgt;
static gtod_fn gtod;
static time_fn vtime;
static getcpu_fn vgetcpu;
static getres_fn getres;
static int failed;

static void check(const char *label, int ok) {
  printf("%s: %s\n", label, ok ? "yes" : "no");
  if (!ok) failed = 1;
}

static void w32(int off, uint32_t v) { *(volatile uint32_t *)(vv + off) = v; }
static void w64(int off, uint64_t v) { *(volatile uint64_t *)(vv + off) = v; }
static uint32_t r32(int off) { return *(volatile uint32_t *)(vv + off); }

struct line { uint64_t base_tsc, base_ns, mult; int64_t real_off; };

/* The kernel's writer protocol (proc::vdso::write): odd, fields, even. */
static void publish(uint32_t mode, const struct line *l, uint32_t flags) {
  uint32_t s = r32(SEQ);
  w32(SEQ, s + 1);
  __asm__ volatile("" ::: "memory");
  w32(MODE, mode);
  w64(BASE_TSC, l->base_tsc);
  w64(BASE_NS, l->base_ns);
  w64(MULT, l->mult);
  w64(REAL_OFF, (uint64_t)l->real_off);
  w64(TAI_OFF, 0);
  w64(BOOT_OFF, 0);
  w32(RES_NS, 1);
  w32(COARSE_RES_NS, 7);
  w32(FLAGS, flags);
  __asm__ volatile("" ::: "memory");
  w32(SEQ, s + 2);
}

static uint64_t at(const struct line *l, uint64_t tsc) {
  uint64_t d = tsc > l->base_tsc ? tsc - l->base_tsc : 0;
  unsigned __int128 v = (unsigned __int128)d * l->mult >> SHIFT;
  return l->base_ns + (uint64_t)v;
}

static uint64_t tsc_now(void) {
  unsigned aux;
  _mm_lfence();
  return __rdtscp(&aux);
}

static int64_t ts_ns(struct timespec t) { return t.tv_sec * 1000000000ll + t.tv_nsec; }

/* One call of `clk` between two TSC reads: within the line's values there (+ `off`), slack `slack` ns low. */
static int bounded(const struct line *l, clockid_t clk, int64_t off, int slack) {
  struct timespec t;
  uint64_t a = tsc_now();
  int r = cgt(clk, &t);
  uint64_t b = tsc_now();
  int64_t v = ts_ns(t);
  return r == 0 && v >= (int64_t)(at(l, a) + off) - slack && v <= (int64_t)(at(l, b) + off);
}

static uint32_t cpu_flags(void) {
  unsigned a, b, c, d;
  uint32_t f = 0;
  if (__get_cpuid(0x80000001, &a, &b, &c, &d) && (d >> 27 & 1)) f |= F_RDTSCP;
  if (__get_cpuid_count(7, 0, &a, &b, &c, &d) && (c >> 22 & 1)) f |= F_RDPID;
  return f;
}

static struct line A, B;
static volatile int stop;

static void *writer(void *x) {
  long *n = x;
  while (!stop) {
    publish(MODE_TSC, &A, cpu_flags());
    publish(MODE_TSC, &B, cpu_flags());
    *n += 2;
  }
  return 0;
}

static void *reader(void *x) {
  long *bad = x;
  for (int i = 0; i < 300000; i++)
    if (!bounded(&A, CLOCK_MONOTONIC, 0, 2)) (*bad)++;
  return 0;
}

int main(int argc, char **argv) {
  if (argc < 2) return 2;
  FILE *f = fopen(argv[1], "rb");
  if (!f) { perror(argv[1]); return 2; }
  static unsigned char img[1 << 16];
  size_t n = fread(img, 1, sizeof img, f);
  fclose(f);
  size_t pages = (n + 4095) / 4096;
  unsigned char *m = mmap(0, VVAR_SIZE + pages * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
  if (m == MAP_FAILED) return 2;
  memcpy(m + VVAR_SIZE, img, n);
  mprotect(m + VVAR_SIZE, pages * 4096, PROT_READ | PROT_EXEC);
  vv = m;
  void *base = m + VVAR_SIZE;
  cgt = (cgt_fn)vdso_sym(base, "__vdso_clock_gettime", "LINUX_2.6");
  gtod = (gtod_fn)vdso_sym(base, "__vdso_gettimeofday", "LINUX_2.6");
  vtime = (time_fn)vdso_sym(base, "__vdso_time", "LINUX_2.6");
  vgetcpu = (getcpu_fn)vdso_sym(base, "__vdso_getcpu", "LINUX_2.6");
  getres = (getres_fn)vdso_sym(base, "__vdso_clock_getres", "LINUX_2.6");
  check("all five functions found with version LINUX_2.6", cgt && gtod && vtime && vgetcpu && getres);
  check("plain names alias them", vdso_sym(base, "clock_gettime", "LINUX_2.6") == (void *)cgt && vdso_sym(base, "time", 0) == (void *)vtime);
  if (failed) return 1;

  /* An unpublished page (all zero): every call is the host's syscall. */
  struct timespec t, h1, h2;
  syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &h1);
  int r = cgt(CLOCK_MONOTONIC, &t);
  syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &h2);
  check("unpublished: clock_gettime is the syscall", r == 0 && ts_ns(t) >= ts_ns(h1) && ts_ns(t) <= ts_ns(h2));
  check("unpublished: bad pointer is EFAULT", cgt(CLOCK_MONOTONIC, (void *)8) == -EFAULT);
  check("unpublished: clock_getres is the syscall", getres(CLOCK_MONOTONIC, &t) == 0 && t.tv_nsec == 1);

  uint32_t fl = cpu_flags();
  struct line l = { tsc_now() - 3000000000ull, 1000000000000ull, 1431655765ull /* 1/3 ns per tick */, 1700000000000000000ll };
  publish(MODE_TSC, &l, fl);
  int ok = 1, real = 1, others = 1;
  for (int i = 0; i < 10000; i++) {
    ok &= bounded(&l, CLOCK_MONOTONIC, 0, 0) && bounded(&l, CLOCK_MONOTONIC_RAW, 0, 0) && bounded(&l, CLOCK_MONOTONIC_COARSE, 0, 0) && bounded(&l, CLOCK_BOOTTIME, 0, 0);
    real &= bounded(&l, CLOCK_REALTIME, l.real_off, 0) && bounded(&l, CLOCK_REALTIME_COARSE, l.real_off, 0) && bounded(&l, CLOCK_TAI, l.real_off, 0);
  }
  check("MONOTONIC, _RAW, _COARSE, BOOTTIME exactly on the line", ok);
  check("REALTIME, _COARSE, TAI exactly on the line plus the offset", real);
  struct timeval tv;
  struct { int w, d; } tz = { 5, 5 };
  uint64_t a = tsc_now();
  r = gtod(&tv, &tz);
  uint64_t b = tsc_now();
  int64_t us = tv.tv_sec * 1000000ll + tv.tv_usec;
  check("gettimeofday on the line in us, timezone zeroed", r == 0 && us >= (int64_t)(at(&l, a) + l.real_off) / 1000 && us <= (int64_t)(at(&l, b) + l.real_off) / 1000 && tz.w == 0 && tz.d == 0);
  long into = 0;
  a = tsc_now();
  long s = vtime(&into);
  b = tsc_now();
  check("time on the line in seconds", s == into && s >= (long)((at(&l, a) + l.real_off) / 1000000000) && s <= (long)((at(&l, b) + l.real_off) / 1000000000));
  check("clock_getres hres 1 ns, COARSE from the page", getres(CLOCK_MONOTONIC, &t) == 0 && t.tv_nsec == 1 && getres(CLOCK_MONOTONIC_COARSE, &t) == 0 && t.tv_nsec == 7 && getres(CLOCK_REALTIME, 0) == 0);
  /* Clocks only the syscall serves, and errors: the host kernel's answers. */
  others &= cgt(CLOCK_PROCESS_CPUTIME_ID, &t) == 0 && cgt(CLOCK_THREAD_CPUTIME_ID, &t) == 0;
  others &= cgt(100, &t) == -EINVAL && cgt(-1, &t) == -EINVAL && getres(100, &t) == -EINVAL;
  check("CPU clocks and bad ids go to the syscall", others);

  cpu_set_t one;
  CPU_ZERO(&one);
  CPU_SET(0, &one);
  sched_setaffinity(0, sizeof one, &one);
  unsigned vc = 9, vn = 9, sc = 8, sn = 8;
  r = (int)vgetcpu(&vc, &vn, 0);
  syscall(SYS_getcpu, &sc, &sn, 0);
  check("getcpu from TSC_AUX = the host's getcpu", (fl & (F_RDTSCP | F_RDPID)) == 0 || (r == 0 && vc == sc && vn == sn));
  publish(MODE_TSC, &l, fl | F_GETCPU);
  r = (int)vgetcpu(&vc, &vn, 0);
  check("getcpu with F_GETCPU = the host's getcpu", r == 0 && vc == sc && vn == sn);

  /* A TSC behind the base (another CPU a little behind the one that published) reads as the base. */
  struct line ahead = l;
  ahead.base_tsc = tsc_now() + (1ull << 50);
  publish(MODE_TSC, &ahead, fl);
  check("a TSC behind the base reads the base", cgt(CLOCK_MONOTONIC, &t) == 0 && (uint64_t)ts_ns(t) == ahead.base_ns);

  publish(MODE_NONE, &l, fl);
  syscall(SYS_clock_gettime, CLOCK_REALTIME, &h1);
  r = cgt(CLOCK_REALTIME, &t);
  syscall(SYS_clock_gettime, CLOCK_REALTIME, &h2);
  check("MODE_NONE: clock_gettime is the syscall", r == 0 && ts_ns(t) >= ts_ns(h1) && ts_ns(t) <= ts_ns(h2));
  check("MODE_NONE: bad pointers are EFAULT", cgt(CLOCK_MONOTONIC, (void *)8) == -EFAULT && gtod((void *)8, 0) == -EFAULT && vtime((void *)8) == -EFAULT);

  /* A writer stuck mid-update (the count odd for good): the bounded wait ends in the syscall, no hang. */
  publish(MODE_TSC, &l, fl);
  {
    uint32_t s = r32(SEQ);
    w32(SEQ, s + 1);
    syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &h1);
    r = cgt(CLOCK_MONOTONIC, &t);
    syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &h2);
    check("stuck writer: clock_gettime is the syscall", r == 0 && ts_ns(t) >= ts_ns(h1) && ts_ns(t) <= ts_ns(h2));
    check("stuck writer: bad pointer is EFAULT", cgt(CLOCK_MONOTONIC, (void *)8) == -EFAULT);
    w32(SEQ, s + 2);
  }
  check("writer done: the line answers again", bounded(&l, CLOCK_MONOTONIC, 0, 0));

  /* Seqlock torture: A and B describe one line (B's base 100 ms of 3 GHz TSC later, on A's line). */
  A = l;
  A.base_tsc = tsc_now() - 600000000ull;
  B = A;
  B.base_tsc = A.base_tsc + 300000000ull;
  B.base_ns = at(&A, B.base_tsc);
  publish(MODE_TSC, &A, fl);
  CPU_ZERO(&one);
  for (int i = 0; i < CPU_SETSIZE; i++) CPU_SET(i, &one);
  sched_setaffinity(0, sizeof one, &one);
  pthread_t w, rd[8];
  long writes = 0, bad[8] = { 0 };
  pthread_create(&w, 0, writer, &writes);
  for (int i = 0; i < 8; i++) pthread_create(&rd[i], 0, reader, &bad[i]);
  long sum = 0;
  for (int i = 0; i < 8; i++) {
    pthread_join(rd[i], 0);
    sum += bad[i];
  }
  stop = 1;
  pthread_join(w, 0);
  printf("torture: %ld writes, %ld wrong readings\n", writes, sum);
  check("torture: no torn reading", sum == 0 && writes > 1000);
  return failed;
}
