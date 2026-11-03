/* SX-2: uid_map, gid_map and setgroups (linux-6.6/kernel/user_namespace.c map_write :920, the second write
 * :966, the writer's namespace :1116, new_idmap_permitted :1160/:1181, proc_setgroups_write :1213/:1252) as
 * Chromium writes them, the hostile writes, and how ids look inside afterwards (stat, /proc/self/status,
 * setuid, setgroups). Shaped like the user-namespace parts of linux-6.6/tools/testing/selftests. The outer
 * uid/gid appear only as "U"/"G". */
#include "ns.h"
#include <grp.h>
#include <sys/uio.h>

static unsigned U, G;

/* The map text with the outer ids replaced: "0 U 1". */
static void show_map(const char *label, const char *path) {
    char b[512], out[512] = "";
    long n = rd(path, b, sizeof b);
    if (n < 0) {
        printf("%s: read = -%s\n", label, en(errno));
        return;
    }
    for (char *line = strtok(b, "\n"); line; line = strtok(0, "\n")) {
        unsigned a, l, c;
        if (sscanf(line, "%u %u %u", &a, &l, &c) == 3) {
            char w[96];
            snprintf(w, sizeof w, "[%u %s %u]", a, l == U ? "U" : l == G ? "G" : "other", c);
            strcat(out, w);
        }
    }
    printf("%s: %ld bytes %s\n", label, n, out);
}

static void status_ids(const char *key) {
    char b[4096];
    rd("/proc/self/status", b, sizeof b);
    char *p = strstr(b, key);
    char *e = p ? strchr(p, '\n') : 0;
    if (e)
        *e = 0;
    printf("%s\n", p ? p : key);
}

static int chromium(void) {
    char t[64];
    R("unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    R("access(/proc/self/setgroups)", access("/proc/self/setgroups", F_OK));
    R("access(/proc/self/ns/user)", access("/proc/self/ns/user", F_OK));
    show_map("uid_map unwritten", "/proc/self/uid_map");
    char b[64];
    rd("/proc/self/setgroups", b, sizeof b);
    printf("setgroups: %s", b);
    snprintf(t, sizeof t, "%u %u 1\n", G, G);
    wr("gid_map before setgroups deny", "/proc/self/gid_map", t);
    wr("setgroups deny", "/proc/self/setgroups", "deny");
    rd("/proc/self/setgroups", b, sizeof b);
    printf("setgroups: %s", b);
    wr("uid_map of another id", "/proc/self/uid_map", "0 0 1\n");
    snprintf(t, sizeof t, "%u %u 2\n", U, U);
    wr("uid_map with count 2", "/proc/self/uid_map", t);
    wr("uid_map bad syntax", "/proc/self/uid_map", "x\n");
    wr("uid_map with a missing field", "/proc/self/uid_map", "0 1000\n");
    static char many[8192];
    many[0] = 0;
    for (int i = 0; i < 341; i++) {
        char l[24];
        snprintf(l, sizeof l, "%d %d 1\n", i * 2, i * 2);
        strcat(many, l);
    }
    wr("uid_map with 341 lines", "/proc/self/uid_map", many);
    int fd = open("/proc/self/uid_map", O_WRONLY);
    snprintf(t, sizeof t, "0 %u 1\n", U);
    lseek(fd, 1, SEEK_SET);
    R("uid_map written after lseek(1)", write(fd, t, strlen(t)));
    close(fd);
    snprintf(t, sizeof t, "0 %u 1\n", G);
    wr("gid_map", "/proc/self/gid_map", t);
    wr("gid_map again", "/proc/self/gid_map", t);
    wr("setgroups allow after deny", "/proc/self/setgroups", "allow");
    wr("setgroups deny after the gid_map", "/proc/self/setgroups", "deny");
    snprintf(t, sizeof t, "0 %u 1\n", U);
    wr("uid_map", "/proc/self/uid_map", t);
    wr("uid_map again", "/proc/self/uid_map", t);
    show_map("uid_map", "/proc/self/uid_map");
    show_map("gid_map", "/proc/self/gid_map");
    printf("ids: uid %d euid %d gid %d egid %d\n", (int)getuid(), (int)geteuid(), (int)getgid(), (int)getegid());
    status_ids("Uid:");
    status_ids("Gid:");
    R("setgroups(0, NULL) with setgroups denied", setgroups(0, 0));
    R("setuid(0)", setuid(0));
    R("setuid(5) unmapped", setuid(5));
    R("setresgid(0, 0, 0)", setresgid(0, 0, 0));
    struct stat st;
    if (stat("/", &st) == 0)
        printf("stat(/): uid %u gid %u\n", (unsigned)st.st_uid, (unsigned)st.st_gid);
    if (stat("/proc/self", &st) == 0)
        printf("stat(/proc/self): uid %u gid %u\n", (unsigned)st.st_uid, (unsigned)st.st_gid);
    struct statx sx;
    if (statx(AT_FDCWD, "/", 0, STATX_BASIC_STATS, &sx) == 0)
        printf("statx(/): uid %u gid %u\n", sx.stx_uid, sx.stx_gid);
    return 0;
}

/* Mode "pwrite" (FS-16): seq_file files clear FMODE_PWRITE (linux-6.6/fs/seq_file.c seq_open): pwrite is ESPIPE
 * before the mode check (fs/read_write.c ksys_pwrite64), write(2) at the position works; a file without
 * seq_file (oom_score_adj) keeps pwrite. */
static int pwrites(void) {
    char t[64];
    int fd = open("/proc/self/maps", O_RDONLY);
    R("maps (read-only): pwrite", pwrite(fd, "x", 1, 0));
    R("maps (read-only): write", write(fd, "x", 1));
    char b[16];
    R("maps: pread of 4 bytes", pread(fd, b, 4, 0));
    close(fd);
    char comm[32];
    long n = rd("/proc/self/comm", comm, sizeof comm);
    if (n > 0 && comm[n - 1] == '\n')
        n--;
    fd = open("/proc/self/comm", O_RDWR);
    R("comm: pwrite", pwrite(fd, comm, n, 0));
    struct iovec v = { comm, (size_t)n };
    R("comm: pwritev", pwritev(fd, &v, 1, 0));
    R("comm: pwritev2 at -1", syscall(SYS_pwritev2, fd, &v, 1, -1L, 0L, 0));
    R("comm: write the same name", write(fd, comm, n));
    close(fd);
    char adj[16];
    long an = rd("/proc/self/oom_score_adj", adj, sizeof adj);
    fd = open("/proc/self/oom_score_adj", O_RDWR);
    printf("oom_score_adj (no seq_file): pwrite of its own value = %s\n", yn(an > 0 && pwrite(fd, adj, an, 0) == an));
    close(fd);
    /* INT-W21 (FS-16 CR to AB): /proc/self/mem keeps pwrite (no seq_file), the position is the address
     * (linux-6.6/fs/proc/base.c mem_rw); an address with nothing mapped is EIO. */
    static volatile char cell[8] = "abcdefg";
    fd = open("/proc/self/mem", O_RDWR);
    printf("mem: pwrite at the address = %s\n", yn(pwrite(fd, "XY", 2, (off_t)(unsigned long)&cell[2]) == 2));
    printf("mem: the memory now = %s\n", (const char *)cell);
    R("mem: pwrite at address 0", pwrite(fd, "x", 1, 0));
    close(fd);
    R("unshare(CLONE_NEWUSER)", unshare(CLONE_NEWUSER));
    fd = open("/proc/self/uid_map", O_WRONLY);
    snprintf(t, sizeof t, "0 %u 1\n", U);
    R("uid_map: pwrite at 0", pwrite(fd, t, strlen(t), 0));
    printf("uid_map: write of the whole line = %s\n", yn(write(fd, t, strlen(t)) == (long)strlen(t)));
    close(fd);
    show_map("uid_map", "/proc/self/uid_map");
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IOLBF, 0);
    U = geteuid();
    G = getegid();
    if (argc > 1 && !strcmp(argv[1], "pwrite"))
        return pwrites();
    char t[64];
    wr("uid_map of the initial namespace", "/proc/self/uid_map", "0 0 1\n");
    show_map("uid_map of the initial namespace", "/proc/self/uid_map");
    pid_t p = fork();
    if (p == 0)
        _exit(chromium());
    reap("chromium's writes", p);

    /* The parent namespace writes a child's map; a sibling namespace may not. */
    int go[2], ready[2];
    if (pipe(go) || pipe(ready))
        return 1;
    pid_t a = fork();
    if (a == 0) {
        close(go[1]);
        unshare(CLONE_NEWUSER);
        if (write(ready[1], "r", 1) != 1)
            _exit(1);
        char c;
        if (read(go[0], &c, 1) != 1)
            _exit(1);
        printf("target: uid %d after the parent wrote its map\n", (int)getuid());
        _exit(0);
    }
    close(go[0]);
    char c;
    if (read(ready[0], &c, 1) != 1)
        return 1;
    char path[64];
    snprintf(path, sizeof path, "/proc/%d/uid_map", (int)a);
    pid_t s = fork();
    if (s == 0) {
        unshare(CLONE_NEWUSER);
        snprintf(t, sizeof t, "0 %u 1\n", U);
        wr("sibling namespace writes the target's uid_map", path, t);
        _exit(0);
    }
    reap("sibling", s);
    snprintf(t, sizeof t, "0 %u 1\n", U);
    wr("parent namespace writes the target's uid_map", path, t);
    if (write(go[1], "g", 1) != 1)
        return 1;
    reap("target", a);
    return 0;
}
