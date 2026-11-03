/* AB-14: the vDSO, the body of abi-vdso (musl, static-PIE: c/vdso.c) and dyn-vdso (glibc, dynamic: dyn/vdso.c).
 * Every answer of the vDSO, called directly (found through AT_SYSINFO_EHDR as the loaders find it) and through
 * the C library, is checked against the raw syscall made right before and right after it, so only relations
 * are printed. Modes (argv[1]):
 *   (none)            the ABI diff: symbols, maps, every function against the syscall, fork
 *   compare <secs>    vDSO and syscall stay ordered (s1 <= vDSO <= s2) for <secs> seconds (ktest ab.vdso.compare)
 *   torture <secs> <threads>   the same from <threads> threads at once while the kernel republishes the page
 *   fallback          the kernel marked the TSC unusable: the vDSO makes the syscalls (a bad pointer is EFAULT)
 *   bench             ns per call: vDSO, libc and raw clock_gettime, vDSO getcpu (printed, never judged)
 * Exit 0 when every check held. */
#include "abi.h"
#include "vdsosym.h"
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/auxv.h>
#include <sys/time.h>
#include <sys/wait.h>

typedef int (*cgt_fn)(clockid_t, struct timespec *);
typedef int (*gtod_fn)(struct timeval *, void *);
typedef long (*time_fn)(long *);
typedef long (*getcpu_fn)(unsigned *, unsigned *, void *);
typedef int (*getres_fn)(clockid_t, struct timespec *);

static cgt_fn v_cgt;
static gtod_fn v_gtod;
static time_fn v_time;
static getcpu_fn v_getcpu;
static getres_fn v_getres;
static int failed;

static void check(const char *label, int cond) {
  yes(label, cond);
  if (!cond) failed = 1;
}

static long long ts_ns(struct timespec t) { return t.tv_sec * 1000000000ll + t.tv_nsec; }

static long long raw_ns(clockid_t c) {
  struct timespec t;
  if (syscall(SYS_clock_gettime, c, &t)) return -1;
  return ts_ns(t);
}

static void find(void) {
  void *base = (void *)getauxval(AT_SYSINFO_EHDR);
  check("AT_SYSINFO_EHDR", base != 0);
  check("vdso ELF header", base && !memcmp(base, ELFMAG, SELFMAG));
  static const char *names[] = { "__vdso_clock_gettime", "__vdso_gettimeofday", "__vdso_time", "__vdso_getcpu", "__vdso_clock_getres",
                                 "clock_gettime", "gettimeofday", "time", "getcpu", "clock_getres" };
  char label[96];
  for (unsigned i = 0; i < sizeof names / sizeof names[0]; i++) {
    snprintf(label, sizeof label, "symbol %s@LINUX_2.6", names[i]);
    check(label, vdso_sym(base, names[i], "LINUX_2.6") != 0);
  }
  check("no symbol __vdso_nonexistent", vdso_sym(base, "__vdso_nonexistent", 0) == 0);
  check("plain clock_gettime is __vdso_clock_gettime", vdso_sym(base, "clock_gettime", 0) == vdso_sym(base, "__vdso_clock_gettime", 0));
  v_cgt = (cgt_fn)vdso_sym(base, "__vdso_clock_gettime", "LINUX_2.6");
  v_gtod = (gtod_fn)vdso_sym(base, "__vdso_gettimeofday", "LINUX_2.6");
  v_time = (time_fn)vdso_sym(base, "__vdso_time", "LINUX_2.6");
  v_getcpu = (getcpu_fn)vdso_sym(base, "__vdso_getcpu", "LINUX_2.6");
  v_getres = (getres_fn)vdso_sym(base, "__vdso_clock_getres", "LINUX_2.6");
  if (!v_cgt || !v_gtod || !v_time || !v_getcpu || !v_getres) {
    printf("vdso functions missing: stop\n");
    exit(1);
  }
}

/* /proc/self/maps: [vvar] read-only below [vdso] (Linux 6.13+ adds [vvar_vclock] between them), [vdso] r-x at
 * AT_SYSINFO_EHDR, the vvar pages ending where the vDSO starts. */
static void maps(void) {
  FILE *f = fopen("/proc/self/maps", "r");
  char line[512], perms[8], name[256];
  unsigned long lo, hi, vvar_lo = 0, vvar_end = 0, vdso_lo = 0, vdso_hi = 0;
  int vvar_ro = 0, vdso_rx = 0;
  while (f && fgets(line, sizeof line, f)) {
    name[0] = 0;
    if (sscanf(line, "%lx-%lx %7s %*s %*s %*s %255s", &lo, &hi, perms, name) < 3) continue;
    if (!strncmp(name, "[vvar", 5)) {
      if (!vvar_lo) vvar_lo = lo;
      vvar_end = hi;
      vvar_ro = !strcmp(perms, "r--p");
    } else if (!strcmp(name, "[vdso]")) {
      vdso_lo = lo;
      vdso_hi = hi;
      vdso_rx = !strcmp(perms, "r-xp");
    }
  }
  if (f) fclose(f);
  check("maps [vvar] r--p", vvar_lo && vvar_ro);
  check("maps [vdso] r-xp", vdso_lo && vdso_rx);
  check("maps vvar pages end at [vdso]", vvar_end && vvar_end == vdso_lo);
  check("maps [vdso] at AT_SYSINFO_EHDR", vdso_lo == getauxval(AT_SYSINFO_EHDR));
  check("maps [vdso] 1 to 4 pages", vdso_hi > vdso_lo && vdso_hi - vdso_lo <= 4 * 4096);
}

/* s1 <= f() <= s2 for clock `c` read through `fn` (the vDSO directly or libc). */
static int between(clockid_t c, cgt_fn fn) {
  struct timespec t;
  long long s1 = raw_ns(c);
  int r = fn(c, &t);
  long long s2 = raw_ns(c);
  long long v = ts_ns(t);
  return r == 0 && t.tv_nsec >= 0 && t.tv_nsec < 1000000000 && s1 <= v && v <= s2;
}

/* A COARSE clock may lag the exact one by a tick (Linux) or be exact (V2): within 50 ms of the syscalls. */
static int near(clockid_t c, cgt_fn fn) {
  struct timespec t;
  long long s1 = raw_ns(c);
  int r = fn(c, &t);
  long long s2 = raw_ns(c);
  long long v = ts_ns(t);
  return r == 0 && v >= s1 - 50000000ll && v <= s2 + 50000000ll;
}

static int libc_cgt(clockid_t c, struct timespec *t) { return clock_gettime(c, t); }

static void clocks(void) {
  static const struct { int id; const char *name; } exact[] = {
    { CLOCK_REALTIME, "REALTIME" }, { CLOCK_MONOTONIC, "MONOTONIC" }, { CLOCK_MONOTONIC_RAW, "MONOTONIC_RAW" },
    { CLOCK_BOOTTIME, "BOOTTIME" }, { CLOCK_TAI, "TAI" } };
  char label[96];
  for (unsigned i = 0; i < sizeof exact / sizeof exact[0]; i++) {
    int ok_v = 1, ok_l = 1;
    for (int k = 0; k < 1000; k++) {
      ok_v &= between(exact[i].id, v_cgt);
      ok_l &= between(exact[i].id, libc_cgt);
    }
    snprintf(label, sizeof label, "vdso clock_gettime(%s) between syscalls", exact[i].name);
    check(label, ok_v);
    snprintf(label, sizeof label, "libc clock_gettime(%s) between syscalls", exact[i].name);
    check(label, ok_l);
  }
  check("vdso clock_gettime(REALTIME_COARSE) near the syscall", near(CLOCK_REALTIME_COARSE, v_cgt));
  check("vdso clock_gettime(MONOTONIC_COARSE) near the syscall", near(CLOCK_MONOTONIC_COARSE, v_cgt));
  check("libc clock_gettime(MONOTONIC_COARSE) near the syscall", near(CLOCK_MONOTONIC_COARSE, libc_cgt));
  /* Clocks the vDSO hands to the kernel: the syscall's answers. */
  struct timespec t;
  check("vdso clock_gettime(PROCESS_CPUTIME) = 0", v_cgt(CLOCK_PROCESS_CPUTIME_ID, &t) == 0);
  check("vdso clock_gettime(THREAD_CPUTIME) = 0", v_cgt(CLOCK_THREAD_CPUTIME_ID, &t) == 0);
  check("vdso clock_gettime(BOOTTIME_ALARM) = 0", v_cgt(9, &t) == 0);
  printf("vdso clock_gettime(100) = %s\n", v_cgt(100, &t) == -EINVAL ? "-EINVAL" : "other");
  printf("vdso clock_gettime(-1) = %s\n", v_cgt(-1, &t) == -EINVAL ? "-EINVAL" : "other");
  printf("libc clock_gettime(100) = %s\n", clock_gettime(100, &t) == -1 && errno == EINVAL ? "-1 EINVAL" : "other");
  long long m1 = 0;
  int mono = 1;
  for (int k = 0; k < 100000; k++) {
    v_cgt(CLOCK_MONOTONIC, &t);
    long long m = ts_ns(t);
    mono &= m >= m1;
    m1 = m;
  }
  check("vdso MONOTONIC never goes back over 100000 calls", mono);
}

static void others(void) {
  struct timeval tv;
  struct timezone tz = { 77, 77 };
  long long s1 = raw_ns(CLOCK_REALTIME);
  int r = v_gtod(&tv, &tz);
  long long s2 = raw_ns(CLOCK_REALTIME);
  long long us = tv.tv_sec * 1000000ll + tv.tv_usec;
  check("vdso gettimeofday between syscalls", r == 0 && tv.tv_usec >= 0 && tv.tv_usec < 1000000 && us >= s1 / 1000 && us <= s2 / 1000);
  check("vdso gettimeofday timezone = syscall's", ({ struct timezone k = { 55, 55 }; syscall(SYS_gettimeofday, 0, &k); k.tz_minuteswest == tz.tz_minuteswest && k.tz_dsttime == tz.tz_dsttime; }));
  check("vdso gettimeofday(NULL, NULL) = 0", v_gtod(0, 0) == 0);
  s1 = raw_ns(CLOCK_REALTIME);
  r = gettimeofday(&tv, 0);
  s2 = raw_ns(CLOCK_REALTIME);
  us = tv.tv_sec * 1000000ll + tv.tv_usec;
  check("libc gettimeofday between syscalls", r == 0 && us >= s1 / 1000 && us <= s2 / 1000);
  long t1 = syscall(SYS_time, 0), into = 0;
  long v = v_time(&into);
  long t2 = syscall(SYS_time, 0);
  check("vdso time between syscalls", v >= t1 && v <= t2 && into == v);
  check("vdso time(NULL) works", v_time(0) >= t1);
  t1 = syscall(SYS_time, 0);
  v = time(0);
  t2 = syscall(SYS_time, 0);
  check("libc time between syscalls", v >= t1 && v <= t2);

  /* getcpu on one CPU: the vDSO, libc and the syscall agree. */
  cpu_set_t one;
  CPU_ZERO(&one);
  CPU_SET(0, &one);
  check("pinned to CPU 0", sched_setaffinity(0, sizeof one, &one) == 0);
  unsigned vc = 99, vn = 99, sc = 98, sn = 98;
  check("vdso getcpu = 0", v_getcpu(&vc, &vn, 0) == 0);
  syscall(SYS_getcpu, &sc, &sn, 0);
  check("vdso getcpu cpu = syscall's", vc == sc && vc == 0);
  check("vdso getcpu node = syscall's", vn == sn);
  check("libc sched_getcpu = syscall's", sched_getcpu() == (int)sc);
  check("vdso getcpu(NULL, NULL) = 0", v_getcpu(0, 0, 0) == 0);

  static const int ids[] = { CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_REALTIME_COARSE, CLOCK_MONOTONIC_COARSE, CLOCK_BOOTTIME, CLOCK_TAI, CLOCK_PROCESS_CPUTIME_ID };
  int same = 1;
  for (unsigned i = 0; i < sizeof ids / sizeof ids[0]; i++) {
    struct timespec a = { 1, 1 }, b = { 2, 2 };
    same &= v_getres(ids[i], &a) == 0 && syscall(SYS_clock_getres, ids[i], &b) == 0 && a.tv_sec == b.tv_sec && a.tv_nsec == b.tv_nsec;
  }
  check("vdso clock_getres = syscall's for every clock", same);
  check("vdso clock_getres(MONOTONIC, NULL) = 0", v_getres(CLOCK_MONOTONIC, 0) == 0);
  struct timespec rs;
  printf("vdso clock_getres(100) = %s\n", v_getres(100, &rs) == -EINVAL ? "-EINVAL" : "other");
}

/* A bad pointer: the fast path faults (SIGSEGV, as on Linux), the syscall answers EFAULT. */
static const char *bad_pointer(void) {
  pid_t p = fork();
  if (p == 0) {
    int r = v_cgt(CLOCK_MONOTONIC, (struct timespec *)8);
    _exit(r == -EFAULT ? 3 : 4);
  }
  int st = 0;
  waitpid(p, &st, 0);
  if (WIFSIGNALED(st)) return WTERMSIG(st) == SIGSEGV ? "SIGSEGV" : "other signal";
  return WEXITSTATUS(st) == 3 ? "-EFAULT" : "other";
}

static void after_fork(void) {
  pid_t p = fork();
  if (p == 0) _exit(between(CLOCK_MONOTONIC, v_cgt) && between(CLOCK_REALTIME, v_cgt) ? 0 : 1);
  int st = 0;
  waitpid(p, &st, 0);
  check("fork child: vdso between syscalls", WIFEXITED(st) && WEXITSTATUS(st) == 0);
}

static int compare(double secs) {
  long long end = raw_ns(CLOCK_MONOTONIC) + (long long)(secs * 1e9);
  long n = 0, bad = 0;
  while (raw_ns(CLOCK_MONOTONIC) < end) {
    for (int k = 0; k < 1000; k++, n++) {
      if (!between(k & 1 ? CLOCK_REALTIME : CLOCK_MONOTONIC, v_cgt)) bad++;
    }
  }
  printf("compare: %ld rounds, %ld out of order\n", n, bad);
  return bad == 0 && n > 0;
}

static double t_secs;
static void *torture_thread(void *arg) {
  long *bad = arg;
  long long end = raw_ns(CLOCK_MONOTONIC) + (long long)(t_secs * 1e9), last = 0;
  struct timespec t;
  while (raw_ns(CLOCK_MONOTONIC) < end) {
    for (int k = 0; k < 200; k++) {
      if (!between(CLOCK_MONOTONIC, v_cgt)) (*bad)++;
      v_cgt(CLOCK_MONOTONIC, &t);
      if (ts_ns(t) < last) (*bad)++;
      last = ts_ns(t);
    }
  }
  return 0;
}

static int torture(double secs, int threads) {
  t_secs = secs;
  pthread_t th[64];
  long bad[64] = { 0 };
  if (threads < 1) threads = 1;
  if (threads > 64) threads = 64;
  for (int i = 0; i < threads; i++) pthread_create(&th[i], 0, torture_thread, &bad[i]);
  long sum = 0;
  for (int i = 0; i < threads; i++) {
    pthread_join(th[i], 0);
    sum += bad[i];
  }
  printf("torture: %d threads, %ld wrong readings\n", threads, sum);
  return sum == 0;
}

static int fallback(void) {
  check("fallback: vdso clock_gettime(MONOTONIC) between syscalls", between(CLOCK_MONOTONIC, v_cgt));
  check("fallback: vdso clock_gettime(REALTIME) between syscalls", between(CLOCK_REALTIME, v_cgt));
  check("fallback: libc clock_gettime(MONOTONIC) between syscalls", between(CLOCK_MONOTONIC, libc_cgt));
  check("fallback: vdso clock_gettime bad pointer = -EFAULT", !strcmp(bad_pointer(), "-EFAULT"));
  check("fallback: vdso gettimeofday bad pointer = -EFAULT", v_gtod((struct timeval *)8, 0) == -EFAULT);
  check("fallback: vdso time bad pointer = -EFAULT", v_time((long *)8) == -EFAULT);
  return !failed;
}

/* Cost per call in ns (no judgement: QEMU's numbers mean little; the desk reads the Mini-PC's). */
static int bench(void) {
  struct timespec t;
  unsigned c, nd;
  const long n = 1000000;
  long long a = raw_ns(CLOCK_MONOTONIC);
  for (long i = 0; i < n; i++) v_cgt(CLOCK_MONOTONIC, &t);
  long long b = raw_ns(CLOCK_MONOTONIC);
  for (long i = 0; i < n; i++) clock_gettime(CLOCK_REALTIME, &t);
  long long d = raw_ns(CLOCK_MONOTONIC);
  for (long i = 0; i < n / 10; i++) syscall(SYS_clock_gettime, CLOCK_MONOTONIC, &t);
  long long e = raw_ns(CLOCK_MONOTONIC);
  for (long i = 0; i < n; i++) v_getcpu(&c, &nd, 0);
  long long f = raw_ns(CLOCK_MONOTONIC);
  printf("bench: vdso clock_gettime %.1f ns, libc clock_gettime %.1f ns, syscall %.1f ns, vdso getcpu %.1f ns\n",
         (double)(b - a) / n, (double)(d - b) / n, (double)(e - d) / (n / 10), (double)(f - e) / n);
  return 1;
}

int main(int argc, char **argv) {
  setvbuf(stdout, 0, _IOLBF, 0);
  find();
  if (argc > 2 && !strcmp(argv[1], "compare")) return compare(atof(argv[2])) ? 0 : 1;
  if (argc > 3 && !strcmp(argv[1], "torture")) return torture(atof(argv[2]), atoi(argv[3])) ? 0 : 1;
  if (argc > 1 && !strcmp(argv[1], "fallback")) return fallback() ? 0 : 1;
  if (argc > 1 && !strcmp(argv[1], "bench")) return bench() ? 0 : 1;
  maps();
  clocks();
  others();
  after_fork();
  printf("vdso clock_gettime bad pointer = %s\n", bad_pointer());
  return failed;
}
