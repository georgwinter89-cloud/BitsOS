/* PK-3's test program (host/tests/pk/80-bitsrun.sh): built on the host against its glibc, dynamic, and started
 * through bitsrun on V2. `probe all` prints what the program sees: folder listings, its interpreter (from
 * /proc/self/maps, with the inode), writes inside and outside its data folder, the environment, and a re-exec
 * through /proc/self/exe (Chromium's zygote pattern). `probe child` is the re-exec'd half. */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

static int cmp(const void *a, const void *b) { return strcmp(*(char *const *)a, *(char *const *)b); }

static void ls(const char *d) {
    char *names[4096];
    int n = 0;
    DIR *dir = opendir(d);
    if (!dir) { printf("ls %s = %s\n", d, strerrorname_np(errno)); return; }
    struct dirent *e;
    while ((e = readdir(dir)) && n < 4096)
        if (strcmp(e->d_name, ".") && strcmp(e->d_name, "..")) names[n++] = strdup(e->d_name);
    closedir(dir);
    qsort(names, n, sizeof *names, cmp);
    printf("ls %s =", d);
    for (int i = 0; i < n; i++) printf(" %s", names[i]);
    printf("\n");
}

static void try_write(const char *p) {
    int fd = open(p, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0) { printf("write %s = %s\n", p, strerrorname_np(errno)); return; }
    int ok = write(fd, "x\n", 2) == 2;
    close(fd);
    printf("write %s = %s\n", p, ok ? "ok" : "short");
}

static void exe(const char *who) {
    char b[4096];
    ssize_t n = readlink("/proc/self/exe", b, sizeof b - 1);
    b[n < 0 ? 0 : n] = 0;
    printf("%s exe = %s\n", who, n < 0 ? strerrorname_np(errno) : b);
}

static void interp(void) {
    FILE *f = fopen("/proc/self/maps", "r");
    char l[8192];
    if (!f) { printf("interp = %s\n", strerrorname_np(errno)); return; }
    while (fgets(l, sizeof l, f)) {
        char perms[8], dev[32], path[4096] = "";
        unsigned long lo, hi, off, ino;
        if (sscanf(l, "%lx-%lx %7s %lx %31s %lu %4095s", &lo, &hi, perms, &off, dev, &ino, path) >= 6 && strstr(path, "ld-linux")) {
            printf("interp = %s %lu\n", path, ino);
            break;
        }
    }
    fclose(f);
}

static void env(const char *k) {
    const char *v = getenv(k);
    printf("env %s = %s\n", k, v ? v : "(unset)");
}

static void cat(const char *p) {
    char b[256] = "";
    int fd = open(p, O_RDONLY);
    if (fd < 0) { printf("cat %s = %s\n", p, strerrorname_np(errno)); return; }
    ssize_t n = read(fd, b, sizeof b - 1);
    close(fd);
    b[n < 0 ? 0 : n] = 0;
    for (char *c = b; *c; c++) if (*c == '\n') *c = '|';
    printf("cat %s = %s\n", p, b);
}

int main(int argc, char **argv) {
    if (argc > 1 && !strcmp(argv[1], "child")) { exe("child"); printf("child = ok\n"); return 0; }
    if (argc > 1 && !strcmp(argv[1], "fail")) { try_write("/tmp/probe-fail"); return 3; }
    const char *home = getenv("HOME");
    char p[4096];
    printf("uid = %u\n", (unsigned)getuid());
    ls("/"); ls("/usr"); ls("/usr/bin"); ls("/etc"); ls("/data/appdata"); if (home) ls(home);
    exe("parent");
    interp();
    cat("/etc/probe.conf");
    cat("/etc/passwd");
    try_write("/usr/probe-w"); try_write("/etc/probe-w"); try_write("/probe-w"); try_write("/data/probe-w");
    try_write("/data/appdata/probe-w"); try_write("/apps/probe-w");
    try_write("/data/appdata/probe/w1"); try_write("/tmp/probe-w");
    if (home) {
        snprintf(p, sizeof p, "%s/.local/share/probe/w2", home); try_write(p);
        snprintf(p, sizeof p, "%s/Downloads/probe-w", home); try_write(p);
        snprintf(p, sizeof p, "%s/Documents/probe-w", home); try_write(p);
    }
    cat("/data/appdata/probe/w2");
    env("LD_LIBRARY_PATH"); env("LD_PRELOAD"); env("XDG_CONFIG_HOME"); env("BITSOS_PROGRAM"); env("PATH");
    fflush(stdout);
    pid_t c = fork();
    if (c == 0) {
        char *a[] = {"probe", "child", NULL};
        execv("/proc/self/exe", a);
        printf("reexec = %s\n", strerrorname_np(errno));
        _exit(127);
    }
    int st = 0;
    waitpid(c, &st, 0);
    printf("reexec status = %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : 128 + WTERMSIG(st));
    return 0;
}
