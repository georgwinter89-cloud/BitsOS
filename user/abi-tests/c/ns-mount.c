/* SX-3a: mount namespaces as an unprivileged user, the way bubblewrap builds a container (bubblewrap.c:
 * unshare/clone of user+mount+pid, mount(NULL, "/", MS_SLAVE|MS_REC), a tmpfs base, the new root bound onto
 * itself, pivot_root(base, oldroot), proc/devpts/mqueue/tmpfs mounts, read-only binds by MS_REMOUNT|MS_BIND,
 * umount2(oldroot, MNT_DETACH), pivot_root(".", ".") + umount2(".", MNT_DETACH)), then mountinfo and mounts as
 * Linux prints them (linux-6.6/fs/namespace.c, fs/pnode.c, fs/proc_namespace.c).
 *
 * Modes: (none) the container; "prop" shared/slave/private/unbindable propagation; "errors" the refusals in
 * Linux's order; "clone" clone(CLONE_NEWUSER|CLONE_NEWNS); "overlay" a read-only overlay of two layers;
 * "pty" a pty of a devpts instance of its own; "pidproc" proc in a new pid namespace; "probe" one line telling whether
 * /proc/self/mountinfo shows the caller's own view (the ktest uses it); "sockbind" a socket bound onto a socket;
 * "nslimit" /proc/sys/user (mode, access, writes); "pdeath" the parent-death signal of a pid namespace's init;
 * "fdpath" /proc/self/fd links and getcwd of places the caller's root does not reach; "hardlink" link(2) and
 * linkat(2) on tmpfs as pressure-vessel's --copy-runtime uses them.
 * mountinfo lines: mount and group ids become indexes in order of appearance, a parent outside the listing
 * "-"; mounts the host made (anything not mounted here) show only as "host <point>". */
#include "ns.h"
#include <sys/mount.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include <sys/vfs.h>
#include <dirent.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <sys/prctl.h>

#ifndef MS_MGC_VAL
#define MS_MGC_VAL 0xC0ED0000
#endif

static const char *me(int e) {
    switch (e) {
    case EROFS: return "EROFS";
    case EBUSY: return "EBUSY";
    case ENOTEMPTY: return "ENOTEMPTY";
    case EXDEV: return "EXDEV";
    }
    return en(e);
}

static long M(const char *label, long r) {
    if (r == -1)
        printf("%s = -%s\n", label, me(errno));
    else
        printf("%s = %ld\n", label, r);
    return r;
}

static long pivot(const char *n, const char *o) {
    return syscall(SYS_pivot_root, n, o);
}

/* Mounts made here have sources starting with "sx". */
static int ours(const char *src, const char *type) {
    (void)type;
    return strncmp(src, "sx", 2) == 0;
}

/* The whole file (a seq_file answers a page per read). */
static long slurp(const char *path, char *out, size_t n) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -1;
    size_t got = 0;
    long r;
    while (got + 1 < n && (r = read(fd, out + got, n - 1 - got)) > 0)
        got += r;
    close(fd);
    out[got] = 0;
    return got;
}

/* Super block options without huge= (the host kernel's transparent_hugepage_shmem default, not the ABI). */
static void sbopts(const char *o) {
    char copy[512];
    snprintf(copy, sizeof copy, "%s", o);
    int first = 1;
    for (char *t = strtok(copy, ","); t; t = strtok(0, ",")) {
        if (!strncmp(t, "huge=", 5))
            continue;
        printf("%s%s", first ? "" : ",", t);
        first = 0;
    }
}

/* Index of `v` in `tab` (added when new). */
static int idx(long *tab, int *n, long v) {
    for (int i = 0; i < *n; i++)
        if (tab[i] == v)
            return i;
    tab[*n] = v;
    return (*n)++;
}

/* Whether mountinfo line `l` (unchanged) is one of ours: its source and type after " - ". */
static int ours_line(const char *l) {
    const char *d = strstr(l, " - ");
    char type[64], src[256];
    return d && sscanf(d + 3, "%63s %255s", type, src) == 2 && ours(src, type);
}

static void mountinfo(const char *title, int show_host) {
    static char buf[65536];
    printf("-- mountinfo %s\n", title);
    if (slurp("/proc/self/mountinfo", buf, sizeof buf) <= 0) {
        printf("read = -%s\n", me(errno));
        return;
    }
    long ids[256];
    int nids = 0;
    long groups[256];
    int ngroups = 0;
    /* First pass: our mounts' ids in order of appearance. */
    for (char *l = buf; *l;) {
        char *e = strchr(l, '\n');
        if (e)
            *e = 0;
        if (ours_line(l) && nids < 256)
            idx(ids, &nids, strtol(l, 0, 10));
        if (!e)
            break;
        *e = '\n';
        l = e + 1;
    }
    int line = 0;
    for (char *l = buf; *l;) {
        char *e = strchr(l, '\n');
        if (e)
            *e = 0;
        char *f[64];
        int nf = 0;
        for (char *t = l, *sp; t && nf < 64; t = sp ? sp + 1 : 0) {
            sp = strchr(t, ' ');
            if (sp)
                *sp = 0;
            f[nf++] = t;
        }
        int dash = 0;
        while (dash < nf && strcmp(f[dash], "-"))
            dash++;
        if (nf < 7 || dash + 2 >= nf) {
            printf("bad line %d\n", line);
        } else if (!ours(f[dash + 2], f[dash + 1])) {
            if (show_host)
                printf("host %s\n", f[4]);
        } else {
            long id = strtol(f[0], 0, 10), parent = strtol(f[1], 0, 10);
            int pi = -1;
            for (int i = 0; i < nids; i++)
                if (ids[i] == parent && parent != id)
                    pi = i;
            char par[16];
            if (pi < 0)
                snprintf(par, sizeof par, "-");
            else
                snprintf(par, sizeof par, "%d", pi);
            printf("%d %s %s %s %s", line, par, f[3], f[4], f[5]);
            for (int i = 6; i < dash; i++) {
                char *c = strchr(f[i], ':');
                if (c) {
                    *c = 0;
                    printf(" %s:g%d", f[i], idx(groups, &ngroups, strtol(c + 1, 0, 10)));
                } else {
                    printf(" %s", f[i]);
                }
            }
            printf(" - %s %s ", f[dash + 1], f[dash + 2]);
            sbopts(f[dash + 3]);
            printf("\n");
            line++;
        }
        if (!e)
            break;
        l = e + 1;
    }
}

static void mounts(int show_host) {
    static char buf[65536];
    printf("-- mounts\n");
    if (slurp("/proc/self/mounts", buf, sizeof buf) <= 0)
        return;
    for (char *l = buf, *nl; l && *l; l = nl ? nl + 1 : 0) {
        nl = strchr(l, '\n');
        if (nl)
            *nl = 0;
        char src[256], point[256], type[64], opts[512];
        if (sscanf(l, "%255s %255s %63s %511s", src, point, type, opts) != 4)
            continue;
        if (ours(src, type)) {
            printf("%s %s %s ", src, point, type);
            sbopts(opts);
            printf("\n");
        }
        else if (show_host)
            printf("host %s\n", point);
    }
}

/* A user namespace with the caller's ids mapped onto themselves (bwrap's default), then `flags`. */
static int enter(unsigned long flags) {
    unsigned uid = getuid(), gid = getgid();
    if (unshare(CLONE_NEWUSER | flags)) {
        printf("unshare = -%s\n", me(errno));
        return -1;
    }
    char map[64];
    snprintf(map, sizeof map, "%u %u 1", uid, uid);
    wr("uid_map", "/proc/self/uid_map", map);
    wr("setgroups", "/proc/self/setgroups", "deny");
    snprintf(map, sizeof map, "%u %u 1", gid, gid);
    wr("gid_map", "/proc/self/gid_map", map);
    return 0;
}

static void touch(const char *label, const char *path) {
    int fd = open(path, O_CREAT | O_WRONLY, 0644);
    if (fd < 0) {
        printf("%s = -%s\n", label, me(errno));
        return;
    }
    printf("%s = 0\n", label);
    close(fd);
}

static void exists(const char *label, const char *path) {
    struct stat st;
    printf("%s = %s\n", label, stat(path, &st) == 0 ? "yes" : me(errno));
}

/* The container (the child: pid 1 of the new pid namespace). */
static int container(void) {
    R("getpid", getpid());
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("tmpfs base", mount("sxbase", "/tmp", "tmpfs", MS_NODEV | MS_NOSUID, NULL));
    M("chdir base", chdir("/tmp"));
    M("mkdir newroot", mkdir("newroot", 0755));
    M("mkdir oldroot", mkdir("oldroot", 0755));
    M("bind newroot", mount("newroot", "newroot", NULL, MS_MGC_VAL | MS_SILENT | MS_BIND | MS_REC, NULL));
    M("pivot_root base", pivot("/tmp", "oldroot"));
    M("chdir /", chdir("/"));
    exists("old root under /oldroot", "/oldroot/tmp");
    /* The new root's contents, from the old root where needed. */
    M("mkdir proc", mkdir("/newroot/proc", 0755));
    M("proc", mount("sxproc", "/newroot/proc", "proc", MS_MGC_VAL | MS_NOSUID | MS_NOEXEC | MS_NODEV, NULL));
    M("mkdir dev", mkdir("/newroot/dev", 0755));
    M("dev tmpfs", mount("sxdev", "/newroot/dev", "tmpfs", MS_MGC_VAL | MS_NOSUID | MS_NODEV, "mode=0755"));
    touch("dev/null file", "/newroot/dev/null");
    M("bind dev/null", mount("/oldroot/dev/null", "/newroot/dev/null", NULL, MS_BIND | MS_REC, NULL));
    M("mkdir pts", mkdir("/newroot/dev/pts", 0755));
    M("devpts", mount("sxpts", "/newroot/dev/pts", "devpts", MS_MGC_VAL | MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=620"));
    M("ptmx link", symlink("pts/ptmx", "/newroot/dev/ptmx"));
    M("mkdir mqueue", mkdir("/newroot/dev/mqueue", 0755));
    M("mqueue", mount("sxmq", "/newroot/dev/mqueue", "mqueue", 0, NULL));
    M("mkdir data", mkdir("/newroot/data", 0755));
    M("data tmpfs", mount("sxdata", "/newroot/data", "tmpfs", MS_NOSUID | MS_NODEV, "size=1m"));
    M("mkdir ro", mkdir("/newroot/ro", 0755));
    M("bind data", mount("/newroot/data", "/newroot/ro", NULL, MS_BIND | MS_REC, NULL));
    M("remount ro", mount("none", "/newroot/ro", NULL, MS_SILENT | MS_BIND | MS_REMOUNT | MS_RDONLY | MS_NOSUID | MS_NODEV, NULL));
    touch("write data", "/newroot/data/x");
    touch("write ro", "/newroot/ro/y");
    exists("ro shows data", "/newroot/ro/x");
    /* What the parent view locked stays locked. */
    M("remount locked proc without nosuid", mount(NULL, "/oldroot/proc", NULL, MS_REMOUNT | MS_BIND, NULL));
    M("remount locked proc keeping it", mount(NULL, "/oldroot/proc", NULL, MS_REMOUNT | MS_BIND | MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL));
    M("umount locked proc", umount2("/oldroot/proc", 0));
    M("umount busy oldroot", umount2("/oldroot", 0));
    M("umount oldroot lazily", umount2("/oldroot", MNT_DETACH));
    exists("oldroot empty", "/oldroot/tmp");
    M("chdir newroot", chdir("/newroot"));
    M("pivot_root . .", pivot(".", "."));
    M("umount . lazily", umount2(".", MNT_DETACH));
    M("chdir /", chdir("/"));
    char cwd[256];
    printf("getcwd = %s\n", getcwd(cwd, sizeof cwd) ? cwd : me(errno));
    exists("/proc/self", "/proc/self/status");
    exists("/newroot", "/newroot");
    exists("/data/x", "/data/x");
    struct stat st;
    if (stat("/dev/ptmx", &st) == 0)
        printf("ptmx = %s %u:%u %03o\n", S_ISCHR(st.st_mode) ? "chr" : "other", major(st.st_rdev), minor(st.st_rdev), st.st_mode & 0777);
    else
        printf("ptmx = -%s\n", me(errno));
    char link[64];
    long n = readlink("/proc/self/ns/mnt", link, sizeof link - 1);
    printf("ns/mnt link = %s\n", n > 5 && !strncmp(link, "mnt:[", 5) ? "mnt:[...]" : "bad");
    mountinfo("container", 1);
    mounts(1);
    return 0;
}

static int mode_container(void) {
    if (enter(CLONE_NEWNS | CLONE_NEWPID | CLONE_NEWIPC))
        return 0;
    pid_t pid = fork();
    if (pid == 0) {
        fflush(stdout);
        _exit(container());
    }
    reap("container", pid);
    return 0;
}

static void prop_inside(void) {
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("tmpfs base", mount("sxbase", "/tmp", "tmpfs", MS_NOSUID | MS_NODEV, NULL));
    for (const char *d = "a\0b\0c\0u\0v\0"; *d; d += strlen(d) + 1) {
        char p[32];
        snprintf(p, sizeof p, "/tmp/%s", d);
        mkdir(p, 0755);
    }
    M("tmpfs a", mount("sxa", "/tmp/a", "tmpfs", 0, NULL));
    mkdir("/tmp/a/sub", 0755);
    mkdir("/tmp/a/sub2", 0755);
    M("make a shared", mount(NULL, "/tmp/a", NULL, MS_SHARED, NULL));
    M("bind a to b", mount("/tmp/a", "/tmp/b", NULL, MS_BIND, NULL));
    M("bind a to c", mount("/tmp/a", "/tmp/c", NULL, MS_BIND, NULL));
    M("make c slave", mount(NULL, "/tmp/c", NULL, MS_SLAVE, NULL));
    M("tmpfs on a/sub", mount("sxsub", "/tmp/a/sub", "tmpfs", 0, NULL));
    touch("file in a/sub", "/tmp/a/sub/f");
    exists("peer b/sub/f", "/tmp/b/sub/f");
    exists("slave c/sub/f", "/tmp/c/sub/f");
    M("tmpfs on c/sub2", mount("sxonc", "/tmp/c/sub2", "tmpfs", 0, NULL));
    touch("file in c/sub2", "/tmp/c/sub2/g");
    exists("a/sub2/g from slave", "/tmp/a/sub2/g");
    mountinfo("propagation", 0);
    M("umount a/sub", umount2("/tmp/a/sub", 0));
    exists("peer b/sub/f after", "/tmp/b/sub/f");
    exists("slave c/sub/f after", "/tmp/c/sub/f");
    M("make b private", mount(NULL, "/tmp/b", NULL, MS_PRIVATE, NULL));
    M("tmpfs on a/sub again", mount("sxsub2", "/tmp/a/sub", "tmpfs", 0, NULL));
    touch("file in a/sub again", "/tmp/a/sub/h");
    exists("private b/sub/h", "/tmp/b/sub/h");
    M("tmpfs u", mount("sxu", "/tmp/u", "tmpfs", 0, NULL));
    M("make u unbindable", mount(NULL, "/tmp/u", NULL, MS_UNBINDABLE, NULL));
    M("bind unbindable", mount("/tmp/u", "/tmp/v", NULL, MS_BIND, NULL));
    M("rbind of the base", mount("/tmp", "/tmp/v", NULL, MS_BIND | MS_REC, NULL));
    exists("rbind shows a/sub/h", "/tmp/v/a/sub/h");
    exists("rbind leaves out u's content", "/tmp/v/u/.");
    M("make a rprivate", mount(NULL, "/tmp/a", NULL, MS_PRIVATE | MS_REC, NULL));
    M("move b to v/b", mount("/tmp/b", "/tmp/v/b", NULL, MS_MOVE, NULL));
    M("move into itself", mount("/tmp/v", "/tmp/v/c", NULL, MS_MOVE, NULL));
    mountinfo("after", 0);
}

static int mode_prop(void) {
    if (enter(CLONE_NEWNS))
        return 0;
    prop_inside();
    return 0;
}

static int mode_errors(void) {
    /* Without a user namespace: no CAP_SYS_ADMIN anywhere. */
    M("mount tmpfs", mount("sxe", "/tmp", "tmpfs", 0, NULL));
    M("mount missing dir", mount("sxe", "/nonexistent-sx", "tmpfs", 0, NULL));
    M("umount2 bad flags", umount2("/tmp", 0x100));
    M("umount2 tmp", umount2("/tmp", 0));
    M("pivot_root", pivot("/", "/"));
    /* The strings are copied before the capability check (copy_mount_string, user_path_at), pivot_root checks
     * may_mount first. */
    M("mount bad type pointer", mount("sxe", "/tmp", (const char *)1, 0, NULL));
    M("mount bad dir pointer", mount("sxe", (const char *)1, "tmpfs", 0, NULL));
    M("mount bad data pointer", mount("sxe", "/tmp", "tmpfs", 0, (const void *)1));
    M("umount2 bad pointer", umount2((const char *)1, 0));
    M("pivot_root bad pointer", pivot((const char *)1, "/"));
    M("unshare mnt", unshare(CLONE_NEWNS));
    if (enter(CLONE_NEWNS))
        return 0;
    M("mount bad source pointer", mount((const char *)1, "/tmp", "tmpfs", 0, NULL));
    M("pivot_root bad pointer inside", pivot((const char *)1, "/"));
    M("two propagation flags", mount(NULL, "/", NULL, MS_SHARED | MS_SLAVE, NULL));
    M("propagation not on a mount root", mount(NULL, "/proc/self", NULL, MS_PRIVATE, NULL));
    M("no type", mount("x", "/tmp", NULL, 0, NULL));
    M("unknown type", mount("x", "/tmp", "nosuchfs-sx", 0, NULL));
    M("bad tmpfs option", mount("sxe", "/tmp", "tmpfs", 0, "size=abc"));
    M("forbidden option", mount("sxe", "/tmp", "tmpfs", 0, "nosuid"));
    M("unmapped uid", mount("sxe", "/tmp", "tmpfs", 0, "uid=4242"));
    M("mqueue in the parent's ipc namespace", mount("mqueue", "/tmp", "mqueue", 0, NULL));
    M("proc of the parent's pid namespace", mount("proc", "/tmp", "proc", 0, NULL));
    M("sysfs of the parent's net namespace", mount("sysfs", "/tmp", "sysfs", 0, NULL));
    M("umount locked proc", umount2("/proc", 0));
    M("umount locked proc lazily", umount2("/proc", MNT_DETACH));
    M("move locked proc", mount("/proc", "/tmp", NULL, MS_MOVE, NULL));
    M("bind / without rec", mount("/", "/tmp", NULL, MS_BIND, NULL));
    M("pivot_root / /", pivot("/", "/"));
    M("tmpfs", mount("sxe", "/tmp", "tmpfs", MS_NOSUID, NULL));
    M("mkdir d", mkdir("/tmp/d", 0755));
    touch("file f", "/tmp/f");
    M("bind file onto dir", mount("/tmp/f", "/tmp/d", NULL, MS_BIND, NULL));
    M("bind dir onto file", mount("/tmp/d", "/tmp/f", NULL, MS_BIND, NULL));
    M("pivot_root onto a file", pivot("/tmp/f", "/tmp/d"));
    M("umount2 expire", umount2("/tmp", MNT_EXPIRE));
    M("umount2 expire detach", umount2("/tmp", MNT_EXPIRE | MNT_DETACH));
    M("chdir tmp", chdir("/tmp"));
    M("umount2 cwd inside", umount2("/tmp", 0));
    M("chdir /", chdir("/"));
    M("umount2 tmp", umount2("/tmp", 0));
    M("umount2 not a mount", umount2("/tmp", 0));
    M("remount sb of the parent's file system", mount(NULL, "/", NULL, MS_REMOUNT | MS_RDONLY, NULL));
    M("tmpfs again", mount("sxe", "/tmp", "tmpfs", 0, NULL));
    M("remount own sb read-only", mount(NULL, "/tmp", NULL, MS_REMOUNT | MS_RDONLY, NULL));
    touch("write on read-only sb", "/tmp/g");
    mountinfo("errors", 0);
    /* Last: Linux 6.6 refuses MNT_FORCE without CAP_SYS_ADMIN in the initial namespace (can_umount); newer
     * kernels unmount (expected/sx3.known). */
    M("umount2 force", umount2("/tmp", MNT_FORCE));
    return 0;
}

static int mode_clone(void) {
    char before[64] = "", after[64] = "";
    readlink("/proc/self/ns/mnt", before, sizeof before - 1);
    int p[2];
    if (pipe(p))
        return 1;
    long pid = rclone(CLONE_NEWUSER | CLONE_NEWNS | SIGCHLD);
    if (pid == 0) {
        char c;
        if (read(p[0], &c, 1) != 1)
            _exit(3);
        readlink("/proc/self/ns/mnt", after, sizeof after - 1);
        printf("child: own mount namespace = %s\n", yn(strcmp(before, after) != 0));
        M("child: tmpfs", mount("sxclone", "/tmp", "tmpfs", 0, NULL));
        touch("child: file", "/tmp/sx-clone-file");
        exists("child: sees it", "/tmp/sx-clone-file");
        fflush(stdout);
        _exit(0);
    }
    char map[64];
    snprintf(map, sizeof map, "/proc/%ld/uid_map", pid);
    char line[32];
    snprintf(line, sizeof line, "%u %u 1", getuid(), getuid());
    wr("uid_map of child", map, line);
    snprintf(map, sizeof map, "/proc/%ld/setgroups", pid);
    wr("setgroups of child", map, "deny");
    snprintf(map, sizeof map, "/proc/%ld/gid_map", pid);
    snprintf(line, sizeof line, "%u %u 1", getgid(), getgid());
    wr("gid_map of child", map, line);
    snprintf(map, sizeof map, "/proc/%ld/ns/mnt", pid);
    int fd = open(map, O_RDONLY);
    R("open child's ns/mnt", fd);
    M("setns into it from outside", setns(fd, CLONE_NEWNS));
    close(fd);
    if (write(p[1], "g", 1) != 1)
        return 1;
    reap("child", pid);
    exists("parent: child's file", "/tmp/sx-clone-file");
    return 0;
}

static void put(const char *path, const char *text) {
    int fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
    if (fd >= 0) {
        if (write(fd, text, strlen(text)) < 0)
            printf("write %s = -%s\n", path, me(errno));
        close(fd);
    }
}

/* The names of `dir`, sorted, on one line. */
static void names(const char *label, const char *dir) {
    DIR *d = opendir(dir);
    if (!d) {
        printf("%s = -%s\n", label, me(errno));
        return;
    }
    char all[64][64];
    int n = 0;
    for (struct dirent *e; (e = readdir(d)) && n < 64;)
        if (strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
            snprintf(all[n++], 64, "%s", e->d_name);
    closedir(d);
    for (int i = 0; i < n; i++)
        for (int j = i + 1; j < n; j++)
            if (strcmp(all[j], all[i]) < 0) {
                char t[64];
                memcpy(t, all[i], 64);
                memcpy(all[i], all[j], 64);
                memcpy(all[j], t, 64);
            }
    printf("%s =", label);
    for (int i = 0; i < n; i++)
        printf(" %s", all[i]);
    printf("\n");
}

static int mode_overlay(void) {
    if (enter(CLONE_NEWNS))
        return 0;
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("tmpfs base", mount("sxbase", "/tmp", "tmpfs", 0, NULL));
    for (const char *d = "lo1\0lo2\0m\0lo1/d\0lo2/d\0lo2/e\0lo2/e/in\0"; *d; d += strlen(d) + 1) {
        char p[64];
        snprintf(p, sizeof p, "/tmp/%s", d);
        mkdir(p, 0755);
    }
    put("/tmp/lo1/a", "upper");
    put("/tmp/lo2/a", "lower");
    put("/tmp/lo2/b", "only lower");
    put("/tmp/lo1/d/x", "x");
    put("/tmp/lo2/d/y", "y");
    put("/tmp/lo1/e", "a file over a directory");
    M("one layer", mount("sxov", "/tmp/m", "overlay", 0, "lowerdir=/tmp/lo1"));
    M("no lowerdir", mount("sxov", "/tmp/m", "overlay", 0, ""));
    M("missing layer", mount("sxov", "/tmp/m", "overlay", 0, "lowerdir=/tmp/lo1:/tmp/nope"));
    M("bad option", mount("sxov", "/tmp/m", "overlay", 0, "lowerdir=/tmp/lo1:/tmp/lo2,bogus"));
    M("overlay", mount("sxov", "/tmp/m", "overlay", 0, "lowerdir=/tmp/lo1:/tmp/lo2"));
    char buf[64];
    printf("a = %s\n", rd("/tmp/m/a", buf, sizeof buf) >= 0 ? buf : me(errno));
    printf("b = %s\n", rd("/tmp/m/b", buf, sizeof buf) >= 0 ? buf : me(errno));
    names("m", "/tmp/m");
    names("m/d", "/tmp/m/d");
    struct stat st;
    printf("e is a file = %s\n", stat("/tmp/m/e", &st) == 0 && S_ISREG(st.st_mode) ? "yes" : "no");
    exists("e/in hidden", "/tmp/m/e/in");
    touch("create", "/tmp/m/new");
    M("mkdir", mkdir("/tmp/m/nd", 0755));
    M("unlink", unlink("/tmp/m/a"));
    int fd = open("/tmp/m/a", O_WRONLY);
    printf("open a for writing = %s\n", fd >= 0 ? "0" : me(errno));
    struct statfs sf;
    printf("statfs type = %s\n", statfs("/tmp/m", &sf) == 0 && sf.f_type == 0x794c7630 ? "overlay" : "other");
    static char mi[65536];
    slurp("/proc/self/mountinfo", mi, sizeof mi);
    char *l = strstr(mi, " - overlay sxov ");
    printf("mountinfo overlay = %s\n", !l ? "missing" : !strncmp(l + 16, "ro,", 3) ? "ro" : "rw");
    return 0;
}

/* setns(2) into a mount namespace of the same user namespace: root and cwd go to its root. */
static int mode_setns(void) {
    if (enter(CLONE_NEWNS))
        return 0;
    M("tmpfs", mount("sxA", "/tmp", "tmpfs", 0, NULL));
    put("/tmp/fromA", "a");
    int fd = open("/proc/self/ns/mnt", O_RDONLY);
    R("open own ns/mnt", fd >= 0 ? 0 : -1);
    pid_t pid = fork();
    if (pid == 0) {
        M("child: unshare mnt", unshare(CLONE_NEWNS));
        M("child: umount copy lazily", umount2("/tmp", MNT_DETACH));
        exists("child: fromA after umount", "/tmp/fromA");
        M("child: chdir /proc", chdir("/proc"));
        M("child: setns wrong type", setns(fd, CLONE_NEWNET));
        M("child: setns back", setns(fd, CLONE_NEWNS));
        exists("child: fromA after setns", "/tmp/fromA");
        char cwd[64];
        printf("child: cwd = %s\n", getcwd(cwd, sizeof cwd) ? cwd : me(errno));
        _exit(0);
    }
    reap("child", pid);
    return 0;
}

/* A devpts instance of its own: the pty opened through its ptmx gets its slave in that instance, numbered from 0
 * (devpts_pty_new). */
static int mode_pty(void) {
    if (enter(CLONE_NEWNS))
        return 0;
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("tmpfs", mount("sxpty", "/tmp", "tmpfs", 0, NULL));
    M("mkdir pts", mkdir("/tmp/pts", 0755));
    M("devpts", mount("sxpts", "/tmp/pts", "devpts", MS_NOSUID | MS_NOEXEC, "newinstance,ptmxmode=0666,mode=620"));
    int m = open("/tmp/pts/ptmx", O_RDWR | O_NOCTTY);
    printf("open ptmx = %s\n", m >= 0 ? "ok" : me(errno));
    if (m < 0)
        return 0;
    int zero = 0;
    unsigned n = 9999;
    M("unlockpt", ioctl(m, TIOCSPTLCK, &zero));
    M("TIOCGPTN", ioctl(m, TIOCGPTN, &n));
    printf("pty number = %u\n", n);
    char path[64];
    snprintf(path, sizeof path, "/tmp/pts/%u", n);
    exists("slave in the instance", path);
    int sl = open(path, O_RDWR | O_NOCTTY);
    printf("open slave = %s\n", sl >= 0 ? "ok" : me(errno));
    if (sl >= 0) {
        M("write master", write(m, "hi\n", 3));
        char b[8] = {0};
        long r = read(sl, b, sizeof b - 1);
        printf("slave reads = %ld %s", r, r > 0 ? b : "\n");
        close(sl);
    }
    close(m);
    return 0;
}

/* proc mounted in a new pid namespace shows that namespace (proc_fill_super's pid_ns): its pid 1 is the
 * mounter, /proc/self is 1, and nothing else lives there. */
static int pidproc(void) {
    R("getpid", getpid());
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("tmpfs", mount("sxpp", "/tmp", "tmpfs", 0, NULL));
    M("mkdir p", mkdir("/tmp/p", 0755));
    M("proc", mount("sxproc", "/tmp/p", "proc", MS_NOSUID | MS_NODEV | MS_NOEXEC, NULL));
    char link[32];
    long n = readlink("/tmp/p/self", link, sizeof link - 1);
    link[n > 0 ? n : 0] = 0;
    printf("self = %s\n", n > 0 ? link : me(errno));
    char st[256];
    long pid = -1;
    if (slurp("/tmp/p/1/stat", st, sizeof st) > 0)
        pid = strtol(st, 0, 10);
    printf("1/stat pid = %ld\n", pid);
    int procs = 0;
    DIR *d = opendir("/tmp/p");
    struct dirent *e;
    while (d && (e = readdir(d)))
        procs += e->d_name[0] >= '1' && e->d_name[0] <= '9';
    if (d)
        closedir(d);
    printf("processes = %d\n", procs);
    return 0;
}

static int mode_pidproc(void) {
    if (enter(CLONE_NEWNS | CLONE_NEWPID))
        return 0;
    pid_t pid = fork();
    if (pid == 0) {
        fflush(stdout);
        _exit(pidproc());
    }
    reap("pidproc", pid);
    return 0;
}

static int mode_probe(void) {
    if (enter(CLONE_NEWNS))
        return 0;
    mount("sxprobe", "/tmp", "tmpfs", 0, NULL);
    static char buf[65536];
    slurp("/proc/self/mountinfo", buf, sizeof buf);
    printf("probe: own view in mountinfo = %s\n", yn(strstr(buf, " sxprobe ") != 0));
    return 0;
}

/* SX-3b (bubblewrap's "bind-mount a socket onto a socket", bubblewrap#409): a unix socket's file bound onto
 * another socket's file; the place then is the first socket (stat, connect reaches its listener). */
static int sock_at(const char *path, int listen_too) {
    int s = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = { .sun_family = AF_UNIX };
    snprintf(a.sun_path, sizeof a.sun_path, "%s", path);
    if (s < 0 || bind(s, (struct sockaddr *)&a, sizeof a) || (listen_too && listen(s, 1)))
        return -1;
    return s;
}

static int mode_sockbind(void) {
    if (enter(CLONE_NEWNS))
        return 0;
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("tmpfs", mount("sxsock", "/tmp", "tmpfs", 0, NULL));
    int a = sock_at("/tmp/a.sock", 1), b = sock_at("/tmp/b.sock", 0);
    printf("sockets = %s\n", a >= 0 && b >= 0 ? "made" : me(errno));
    struct stat sa, sb;
    stat("/tmp/a.sock", &sa);
    M("bind a.sock onto b.sock", mount("/tmp/a.sock", "/tmp/b.sock", NULL, MS_BIND, NULL));
    printf("b is a socket = %s\n", stat("/tmp/b.sock", &sb) == 0 ? yn(S_ISSOCK(sb.st_mode)) : me(errno));
    printf("b is a's inode = %s\n", yn(sb.st_ino == sa.st_ino && sb.st_dev == sa.st_dev));
    int c = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un to = { .sun_family = AF_UNIX, .sun_path = "/tmp/b.sock" };
    printf("connect b = %s\n", connect(c, (struct sockaddr *)&to, sizeof to) == 0 ? "0" : me(errno));
    int got = accept(a, 0, 0);
    printf("a accepted = %s\n", got >= 0 ? "yes" : me(errno));
    close(got);
    close(c);
    close(b);
    close(a);
    M("umount b", umount2("/tmp/b.sock", 0));
    return 0;
}

/* SX-3b (bwrap --disable-userns): /proc/sys/user/max_user_namespaces is the reader's user namespace's limit,
 * writable only with CAP_SYS_RESOURCE there; 1 written in a namespace allows one namespace made there, so the
 * second level cannot make a third (every level counts, linux-6.6/kernel/ucount.c inc_ucount). */
static void limit_line(const char *label) {
    char b[64];
    long n = slurp("/proc/sys/user/max_user_namespaces", b, sizeof b);
    if (n > 0 && b[n - 1] == '\n')
        b[n - 1] = 0;
    printf("%s = %s\n", label, n > 0 ? b : me(errno));
}

/* One write's answer: ok (all of it), the byte count (a part) or the errno. */
static void wline(const char *label, int fd, const char *text) {
    long n = (long)strlen(text), r = write(fd, text, n);
    if (r == n)
        printf("%s = ok\n", label);
    else if (r >= 0)
        printf("%s = %ld bytes\n", label, r);
    else
        printf("%s = %s\n", label, me(errno));
}

/* The file's mode and access(W_OK) (Linux: 0644 for everyone, access by CAP_SYS_RESOURCE, proc_sys_permission). */
static void limit_perm(const char *where) {
    struct stat st;
    const char *f = "/proc/sys/user/max_user_namespaces";
    if (stat(f, &st))
        printf("stat %s = %s\n", where, me(errno));
    else
        printf("mode %s = %04o\n", where, (unsigned)(st.st_mode & 07777));
    printf("access W_OK %s = %s\n", where, access(f, W_OK) ? me(errno) : "ok");
    printf("access X_OK %s = %s\n", where, access(f, X_OK) ? me(errno) : "ok");
}

static int mode_nslimit(void) {
    limit_perm("outside");
    int fd = open("/proc/sys/user/max_user_namespaces", O_WRONLY);
    printf("open for write outside = %s\n", fd >= 0 ? "ok" : me(errno));
    if (fd >= 0)
        close(fd);
    if (enter(0))
        return 0;
    limit_perm("inside");
    limit_line("limit in a new namespace");
    fd = open("/proc/sys/user/max_user_namespaces", O_WRONLY);
    printf("open for write inside = %s\n", fd >= 0 ? "ok" : me(errno));
    wline("write 3000000000", fd, "3000000000");
    wline("write -1", fd, "-1");
    wline("write blank", fd, " \n");
    wline("write 1", fd, "1");
    wline("write 5 at offset 1", fd, "5");
    close(fd);
    limit_line("limit after the write");
    pid_t pid = fork();
    if (pid == 0) {
        R("second level", unshare(CLONE_NEWUSER));
        limit_line("limit seen from the second level");
        R("third level", unshare(CLONE_NEWUSER));
        _exit(0);
    }
    reap("nslimit", pid);
    return 0;
}

/* SX-3b: PR_SET_PDEATHSIG of a pid namespace's init whose parent is outside the namespace (bwrap
 * --die-with-parent --unshare-pid): the parent's end kills it, although an init ignores the signals sent from
 * inside its namespace (linux-6.6/kernel/signal.c:1233, SEND_SIG_NOINFO from an invisible sender is forced).
 * The init is reparented to this process (a subreaper) and reaped here; 5 s bound. */
static int mode_pdeath(void) {
    int ready[2], idp[2];
    if (prctl(PR_SET_CHILD_SUBREAPER, 1) || pipe(ready) || pipe(idp)) {
        printf("setup = %s\n", me(errno));
        return 0;
    }
    pid_t b = fork();
    if (b == 0) {
        if (enter(CLONE_NEWPID))
            _exit(1);
        pid_t c = fork();
        if (c == 0) {
            R("pdeathsig", prctl(PR_SET_PDEATHSIG, SIGKILL));
            char one = getpid() == 1 ? '1' : 'x';
            if (write(ready[1], &one, 1) != 1)
                _exit(1);
            for (;;)
                pause();
        }
        char got = 0;
        if (c < 0 || read(ready[0], &got, 1) != 1) {
            printf("init = %s\n", c < 0 ? me(errno) : "no answer");
            _exit(1);
        }
        printf("init is pid 1 = %s\n", yn(got == '1'));
        if (write(idp[1], &c, sizeof c) != sizeof c)
            _exit(1);
        _exit(0);
    }
    close(ready[1]);
    close(idp[1]);
    pid_t c = 0;
    if (read(idp[0], &c, sizeof c) != sizeof c)
        c = 0;
    reap("parent", b);
    if (c <= 0)
        return 0;
    int st = 0, i;
    for (i = 0; i < 50; i++) {
        pid_t r = waitpid(c, &st, WNOHANG);
        if (r == c)
            break;
        if (r < 0) {
            printf("waitpid init = %s\n", me(errno));
            return 0;
        }
        usleep(100000);
    }
    if (i == 50) {
        printf("init after the parent's end = still running after 5 s\n");
        kill(c, SIGKILL);
        waitpid(c, &st, 0);
        return 0;
    }
    if (WIFSIGNALED(st))
        printf("init after the parent's end = signal %d\n", WTERMSIG(st));
    else
        printf("init after the parent's end = exit %d\n", WEXITSTATUS(st));
    return 0;
}

/* SX-3c (pressure-vessel's fds opened before the container): readlink of /proc/self/fd/N and getcwd for places
 * the caller's root does not reach show the path up to the top of the place's own mount chain
 * (linux-6.6/fs/d_path.c __prepend_path; getcwd adds "(unreachable)", d_path.c:432): a file of the view the
 * fd was opened in keeps its whole path there, a file of a lazily unmounted tree the path from its root.
 * The test directory's name (it carries the pid) is printed as D. */
static void fdlink_in(const char *proc, const char *label, int fd, const char *dir) {
    char p[64], l[512];
    snprintf(p, sizeof p, "%s/self/fd/%d", proc, fd);
    long n = readlink(p, l, sizeof l - 1);
    if (n < 0) {
        printf("%s = %s\n", label, me(errno));
        return;
    }
    l[n] = 0;
    char *at = strstr(l, dir);
    if (at)
        printf("%s = %.*sD%s\n", label, (int)(at - l), l, at + strlen(dir));
    else
        printf("%s = %s\n", label, l);
}

/* The same through a directory fd of a /proc/self/fd. */
static void fdlink_at(int pfd, const char *label, int fd, const char *dir) {
    char p[16], l[512];
    snprintf(p, sizeof p, "%d", fd);
    long n = readlinkat(pfd, p, l, sizeof l - 1);
    if (n < 0) {
        printf("%s = %s\n", label, me(errno));
        return;
    }
    l[n] = 0;
    char *at = strstr(l, dir);
    if (at)
        printf("%s = %.*sD%s\n", label, (int)(at - l), l, at + strlen(dir));
    else
        printf("%s = %s\n", label, l);
}

static void fdlink(const char *label, int fd, const char *dir) {
    fdlink_in("/proc", label, fd, dir);
}

/* The system call itself: the C libraries turn "(unreachable)..." into ENOENT. */
static void cwdline(const char *label, const char *dir) {
    char b[512];
    long n = syscall(SYS_getcwd, b, sizeof b);
    char *at = n > 0 ? strstr(b, dir) : 0;
    if (at)
        printf("%s = %.*sD%s\n", label, (int)(at - b), b, at + strlen(dir));
    else
        printf("%s = %s\n", label, n > 0 ? b : me(errno));
}

/* getcwd with a buffer of `n` bytes: its length or the error. */
static void cwdsize(const char *label, size_t n) {
    char b[512];
    M(label, syscall(SYS_getcwd, b, n));
}

static int mode_fdpath(void) {
    char dir[64], f[96];
    snprintf(dir, sizeof dir, "/tmp/sx-fdpath-%d", (int)getpid());
    snprintf(f, sizeof f, "%s/f", dir);
    if (mkdir(dir, 0700)) {
        printf("mkdir = %s\n", me(errno));
        return 0;
    }
    int fd = open(f, O_CREAT | O_RDWR, 0600), dfd = open(dir, O_RDONLY | O_DIRECTORY);
    fdlink("fd before", fd, dir + 5);
    pid_t pid = fork();
    if (pid == 0) {
        if (enter(CLONE_NEWNS))
            _exit(0);
        fdlink("fd in a copied view", fd, dir + 5);
        M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
        M("tmpfs on /tmp", mount("sxfd", "/tmp", "tmpfs", 0, NULL));
        fdlink("fd under the new tmpfs", fd, dir + 5);
        M("mkdir w", mkdir("/tmp/w", 0755));
        M("mkdir w/x", mkdir("/tmp/w/x", 0755));
        int g = open("/tmp/w/g", O_CREAT | O_RDWR, 0600);
        fdlink("g", g, dir + 5);
        M("chdir w/x", chdir("/tmp/w/x"));
        M("umount /tmp lazily", umount2("/tmp", MNT_DETACH));
        fdlink("g after the lazy umount", g, dir + 5);
        cwdline("getcwd after the lazy umount", dir + 5);
        cwdsize("getcwd one byte short", strlen("(unreachable)/w/x"));
        cwdsize("getcwd with room for the NUL", strlen("(unreachable)/w/x") + 1);
        M("chdir /", chdir("/"));
        M("chroot /proc", chroot("/proc"));
        fdlink_in("", "fd after chroot", fd, dir + 5);
        M("chdir /sys", chdir("/sys"));
        cwdline("getcwd in the chroot", dir + 5);
        M("fchdir to the test directory", fchdir(dfd));
        cwdline("getcwd outside the chroot", dir + 5);
        _exit(0);
    }
    reap("fdpath", pid);
    /* After pivot_root (bwrap's order): an fd of the copied view is reached through the old root; an fd of the
     * first view keeps its path there, also after the old root is lazily unmounted. */
    pid = fork();
    if (pid == 0) {
        if (enter(CLONE_NEWNS))
            _exit(0);
        M("pivot: make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
        int f2 = open(f, O_RDONLY);
        M("pivot: tmpfs base on /tmp", mount("sxpv", "/tmp", "tmpfs", 0, NULL));
        M("pivot: mkdir oldroot", mkdir("/tmp/oldroot", 0755));
        M("pivot_root", pivot("/tmp", "/tmp/oldroot"));
        M("pivot: chdir to the old /tmp", chdir("/oldroot/tmp"));
        fdlink_in("/oldroot/proc", "pivot: fd of the copy", f2, dir + 5);
        fdlink_in("/oldroot/proc", "pivot: fd of the first view", fd, dir + 5);
        cwdline("pivot: getcwd", dir + 5);
        /* proc stays readable through a directory fd after its mount is gone. */
        int pfd = open("/oldroot/proc/self/fd", O_RDONLY | O_DIRECTORY);
        M("pivot: umount oldroot lazily", umount2("/oldroot", MNT_DETACH));
        /* Left out: the copy's fd and getcwd after it. The old root's children are locked (lock_mnt_tree), and
         * Linux 6.6 keeps locked children connected on a lazy umount (namespace.c:1617, "/tmp/D/f" and
         * "(unreachable)/tmp") while CachyOS's newer kernel cuts them off ("/D/f", "(unreachable)/"). */
        fdlink_at(pfd, "pivot: fd of the first view after the umount", fd, dir + 5);
        _exit(0);
    }
    reap("fdpath pivot", pid);
    close(dfd);
    close(fd);
    unlink(f);
    rmdir(dir);
    return 0;
}

/* SX-3c: the hard links pressure-vessel's --copy-runtime makes (linux-6.6/fs/namei.c do_linkat, may_linkat,
 * vfs_link), in a tmpfs of a user namespace's own: a second name keeps the data when the first goes, a bind of
 * the same file system is another mount (EXDEV), a read-only bind is EROFS (before EXDEV), an O_TMPFILE file
 * gets its first name through /proc/self/fd, a removed one none. AT_EMPTY_PATH is left to the vfs host test:
 * Linux 6.6 refuses it without CAP_DAC_READ_SEARCH (namei.c:4648), CachyOS's newer kernel allows it for the
 * opener, so the two would differ. */
static void nlinks(const char *label, const char *path) {
    struct stat st;
    if (stat(path, &st))
        printf("%s = -%s\n", label, me(errno));
    else
        printf("%s = %lu\n", label, (unsigned long)st.st_nlink);
}

static void readback(const char *label, const char *path) {
    char b[16] = {0};
    int fd = open(path, O_RDONLY);
    long n = fd < 0 ? -1 : read(fd, b, sizeof b - 1);
    printf("%s = %s\n", label, n < 0 ? me(errno) : b);
    if (fd >= 0)
        close(fd);
}

static int mode_hardlink(void) {
    if (enter(CLONE_NEWNS))
        return 0;
    M("make / rslave", mount(NULL, "/", NULL, MS_SLAVE | MS_REC, NULL));
    M("tmpfs on /tmp", mount("sxhl", "/tmp", "tmpfs", 0, NULL));
    M("mkdir a", mkdir("/tmp/a", 0755));
    M("mkdir b", mkdir("/tmp/b", 0755));
    M("mkdir c", mkdir("/tmp/c", 0755));
    int fd = open("/tmp/a/f", O_CREAT | O_RDWR, 0644);
    M("write f", write(fd, "data", 4));
    close(fd);
    M("link f g", link("/tmp/a/f", "/tmp/a/g"));
    nlinks("nlink f", "/tmp/a/f");
    M("link f g again", link("/tmp/a/f", "/tmp/a/g"));
    M("link a directory", link("/tmp/a", "/tmp/dl"));
    M("link to a trailing slash", link("/tmp/a/f", "/tmp/a/n/"));
    M("link of a missing file", link("/tmp/a/nofile", "/tmp/a/n"));
    M("unlink f", unlink("/tmp/a/f"));
    nlinks("nlink g", "/tmp/a/g");
    readback("g reads", "/tmp/a/g");
    M("bind a on b", mount("/tmp/a", "/tmp/b", NULL, MS_BIND, NULL));
    M("link across the bind", link("/tmp/a/g", "/tmp/b/h"));
    M("link inside the bind", link("/tmp/b/g", "/tmp/b/h"));
    nlinks("nlink of h through a", "/tmp/a/h");
    M("tmpfs on c", mount("sxhl2", "/tmp/c", "tmpfs", 0, NULL));
    M("link to another tmpfs", link("/tmp/a/g", "/tmp/c/g"));
    M("bind read-only", mount(NULL, "/tmp/b", NULL, MS_REMOUNT | MS_BIND | MS_RDONLY, NULL));
    M("link inside the read-only bind", link("/tmp/b/g", "/tmp/b/i"));
    M("link into it from another mount", link("/tmp/a/g", "/tmp/b/i"));
    M("link onto a name in it", link("/tmp/a/g", "/tmp/b/g"));
    int g = open("/tmp/a/g", O_RDONLY);
    M("linkat with a bad flag", linkat(AT_FDCWD, "/tmp/a/g", AT_FDCWD, "/tmp/a/j", 0x2000));
    char p[64];
    snprintf(p, sizeof p, "/proc/self/fd/%d", g);
    M("linkat through /proc/self/fd", linkat(AT_FDCWD, p, AT_FDCWD, "/tmp/a/k", AT_SYMLINK_FOLLOW));
    nlinks("nlink g after it", "/tmp/a/g");
    M("unlink g", unlink("/tmp/a/g"));
    M("unlink h", unlink("/tmp/a/h"));
    M("unlink k", unlink("/tmp/a/k"));
    M("linkat of a removed file", linkat(AT_FDCWD, p, AT_FDCWD, "/tmp/a/l", AT_SYMLINK_FOLLOW));
    close(g);
    int t = open("/tmp/a", O_TMPFILE | O_RDWR, 0600);
    M("O_TMPFILE", t < 0 ? -1 : 0);
    if (t >= 0) {
        M("write it", write(t, "temp", 4));
        snprintf(p, sizeof p, "/proc/self/fd/%d", t);
        M("linkat of O_TMPFILE", linkat(AT_FDCWD, p, AT_FDCWD, "/tmp/a/t", AT_SYMLINK_FOLLOW));
        M("linkat of it again", linkat(AT_FDCWD, p, AT_FDCWD, "/tmp/a/t2", AT_SYMLINK_FOLLOW));
        nlinks("nlink t", "/tmp/a/t");
        readback("t reads", "/tmp/a/t");
        close(t);
    }
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, 0, _IONBF, 0);
    const char *m = argc > 1 ? argv[1] : "";
    if (!strcmp(m, "prop"))
        return mode_prop();
    if (!strcmp(m, "errors"))
        return mode_errors();
    if (!strcmp(m, "clone"))
        return mode_clone();
    if (!strcmp(m, "setns"))
        return mode_setns();
    if (!strcmp(m, "overlay"))
        return mode_overlay();
    if (!strcmp(m, "probe"))
        return mode_probe();
    if (!strcmp(m, "pty"))
        return mode_pty();
    if (!strcmp(m, "pidproc"))
        return mode_pidproc();
    if (!strcmp(m, "sockbind"))
        return mode_sockbind();
    if (!strcmp(m, "nslimit"))
        return mode_nslimit();
    if (!strcmp(m, "pdeath"))
        return mode_pdeath();
    if (!strcmp(m, "fdpath"))
        return mode_fdpath();
    if (!strcmp(m, "hardlink"))
        return mode_hardlink();
    return mode_container();
}
