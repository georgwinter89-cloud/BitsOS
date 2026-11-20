/* BIGDIR: Steam's pattern in one folder as a Linux program, the loop of the kernel test fs.ext4.bigdir:
 * N creates, 200 listings (open, fstat, getdents64 with a 32 KiB buffer), 200000 failed lookups (the name in
 * lower case), failed lookups each followed by a listing (at most 5000 or 10 s), N renames in place and N
 * unlinks in a scattered order. One line with the rates; the folder is removed at the end.
 *
 * The same static program runs on CachyOS and on BitsOS (Linux ABI):
 *   gcc -O2 -static -o bigdir-bench host/tests/fs/tools/bigdir-bench.c
 *   ./bigdir-bench /data/home/1000/bench/d [N, default 50000]
 */
#define _GNU_SOURCE
#include <ctype.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* Names like Steam's: 30 to 60 bytes, mixed case. */
static void name(char *b, const char *d, long i, int gen) {
    sprintf(b, "%s/Civ7_Asset%d_%06ld_Base-Platforms%.*s.blp", d, gen, i, (int)(i % 30), "SharedData_Texture_Atlas_Block");
}

static void lower(char *p) {
    for (; *p; p++) *p = tolower(*p);
}

static long list(const char *d) {
    static char buf[32768];
    struct stat s;
    long n, total = 0;
    int f = open(d, O_RDONLY | O_DIRECTORY);
    if (f < 0 || fstat(f, &s)) { perror("open folder"); exit(1); }
    while ((n = syscall(SYS_getdents64, f, buf, sizeof buf)) > 0) total += n;
    close(f);
    return total;
}

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: %s FOLDER [N]\n", argv[0]); return 2; }
    const char *d = argv[1];
    long n = argc > 2 ? atol(argv[2]) : 50000, i, k, pairs = 0, bytes = 0;
    char a[512], b[512];
    if (mkdir(d, 0755)) { perror("mkdir (the folder must not exist)"); return 1; }

    double t = now();
    for (i = 0; i < n; i++) {
        name(a, d, i, 0);
        int f = open(a, O_RDWR | O_CREAT, 0644);
        if (f < 0) { perror("create"); return 1; }
        close(f);
    }
    double creates = n / (now() - t);

    t = now();
    for (i = 0; i < 200; i++) bytes += list(d);
    double ns_listed = (now() - t) * 1e9 / (200.0 * (n + 2));

    t = now();
    for (i = 0; i < 200000; i++) {
        name(a, d, i % n, 0);
        lower(a + strlen(d));
        if (access(a, F_OK) == 0) { fprintf(stderr, "a name in another case was found\n"); return 1; }
    }
    double misses = 200000 / (now() - t);

    t = now();
    while (pairs < 5000 && now() - t < 10) {
        name(a, d, pairs % n, 0);
        lower(a + strlen(d));
        access(a, F_OK);
        list(d);
        pairs++;
    }
    double pairs_s = pairs / (now() - t);

    t = now();
    for (k = 0; k < n; k++) {
        i = k * 7919 % n;
        name(a, d, i, 0);
        name(b, d, i, 1);
        if (rename(a, b)) { perror("rename"); return 1; }
    }
    double renames = n / (now() - t);

    t = now();
    for (k = 0; k < n; k++) {
        name(a, d, k * 7919 % n, 1);
        if (unlink(a)) { perror("unlink"); return 1; }
    }
    double unlinks = n / (now() - t);
    if (rmdir(d)) { perror("rmdir"); return 1; }

    printf("bigdir files=%ld creates_per_s=%.0f ns_per_listed=%.0f listed_mb=%ld misses_per_s=%.0f miss_then_list=%ld miss_then_list_per_s=%.0f renames_per_s=%.0f unlinks_per_s=%.0f\n",
           n, creates, ns_listed, bytes >> 20, misses, pairs, pairs_s, renames, unlinks);
    return 0;
}
