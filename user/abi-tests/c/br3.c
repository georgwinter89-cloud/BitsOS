/* BR-3: what Brave 1.97.59 (Chromium, --no-sandbox) met on V2 that differs from Linux, one part per argv[1].
 *   tasknlink  /proc/self/task's link count, as Chromium's sandbox::ThreadHelpers::IsSingleThreaded reads it
 *              (fstatat(proc_fd, "self/task/") must give 2 + threads; CHECK_LE(3, st_nlink) kills the zygote:
 *              linux-6.6/fs/proc/base.c:3897-3910 proc_task_getattr)
 *   siocgiw    the wireless-extension ioctls 0x8B00-0x8BFF Chromium's network-change code asks on a UDP socket
 *              (SIOCGIWNAME, SIOCGIWESSID on "lo": EOPNOTSUPP; unknown interface ENODEV; bad pointer EFAULT first;
 *              SET commands and SIOCGIWENCODE unprivileged EPERM before the lookup; net/wireless/wext-core.c)
 *   procnlink  the other /proc directories' link counts: 2 + subdirectories, as readdir lists them
 *              (/proc/self and /proc/self/task/<tid>: base.c:3926-3929 pid_entry_nlink; GNU find and fts skip
 *              the stat of subdirectories by st_nlink)
 *   uevent     AF_NETLINK NETLINK_KOBJECT_UEVENT the way libudev's monitor opens it (Chromium's UdevWatcher binds
 *              the "udev" group 2; group 1 is the kernel's): bind any group unprivileged, SO_PASSCRED,
 *              SO_ATTACH_FILTER, getsockname (lib/kobject_uevent.c:761-763, af_netlink.c:1024, :2040-2041)
 * Every line is a result or an errno name; nothing names a pid, an address or this machine. */
#include "sock.h"
#include <fcntl.h>
#include <net/if.h>
#include <dirent.h>
#include <pthread.h>
#include <sys/wait.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <linux/filter.h>
#include <linux/netlink.h>
#include <linux/wireless.h>

static long task_nlink(int proc) {
    struct stat st;
    if (fstatat(proc, "self/task/", &st, 0) != 0) {
        printf("fstatat self/task = -%s\n", sename(errno));
        return -1;
    }
    return (long)st.st_nlink;
}

static pthread_mutex_t m = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t cv = PTHREAD_COND_INITIALIZER;
static int go;

static void *idle(void *a) {
    (void)a;
    pthread_mutex_lock(&m);
    while (!go)
        pthread_cond_wait(&cv, &m);
    pthread_mutex_unlock(&m);
    return 0;
}

static void tasknlink(void) {
    int proc = open("/proc/", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (proc < 0) {
        printf("open /proc = -%s\n", sename(errno));
        return;
    }
    printf("one thread: nlink = %ld\n", task_nlink(proc));
    pthread_t t[2];
    for (int i = 0; i < 2; i++)
        pthread_create(&t[i], 0, idle, 0);
    printf("three threads: nlink = %ld\n", task_nlink(proc));
    pthread_mutex_lock(&m);
    go = 1;
    pthread_cond_broadcast(&cv);
    pthread_mutex_unlock(&m);
    for (int i = 0; i < 2; i++)
        pthread_join(t[i], 0);
    /* A joined thread leaves the group a moment after its join returns (clear_child_tid comes first). */
    long n = 0;
    for (int i = 0; i < 1000 && (n = task_nlink(proc)) != 3; i++)
        usleep(1000);
    printf("joined: nlink = %ld\n", n);
    struct stat st;
    yes("self/task is a directory", fstatat(proc, "self/task", &st, 0) == 0 && S_ISDIR(st.st_mode));
    close(proc);
}

/* Is st_nlink of `path` 2 + its subdirectories (readdir's DT_DIR entries without "." and "..")? */
static void subdirs(const char *label, const char *path) {
    struct stat st;
    DIR *d = opendir(path);
    if (!d || stat(path, &st) != 0) {
        printf("%s = -%s\n", label, sename(errno));
        if (d)
            closedir(d);
        return;
    }
    long dirs = 0;
    struct dirent *e;
    while ((e = readdir(d)))
        if (e->d_type == DT_DIR && strcmp(e->d_name, ".") && strcmp(e->d_name, ".."))
            dirs++;
    closedir(d);
    yes(label, dirs > 0 && (long)st.st_nlink == 2 + dirs);
}

static void procnlink(void) {
    subdirs("/proc/self: nlink = 2 + subdirectories", "/proc/self");
    char p[64];
    snprintf(p, sizeof p, "/proc/self/task/%ld", (long)gettid());
    subdirs("/proc/self/task/<tid>: nlink = 2 + subdirectories", p);
}

static void iw(int s, const char *label, unsigned long cmd, const char *ifname, void *arg) {
    struct iwreq w;
    memset(&w, 0, sizeof w);
    strcpy(w.ifr_name, ifname);
    char essid[IW_ESSID_MAX_SIZE + 1] = "x";
    w.u.essid.pointer = essid;
    w.u.essid.length = sizeof essid;
    printf("%s = %s\n", label, ioctl(s, cmd, arg ? arg : &w) == 0 ? "0" : sename(errno));
}

static void siocgiw(void) {
    int s = socket(AF_INET, SOCK_DGRAM | SOCK_CLOEXEC, 0);
    iw(s, "SIOCGIWNAME lo", SIOCGIWNAME, "lo", 0);
    iw(s, "SIOCGIWESSID lo", SIOCGIWESSID, "lo", 0);
    iw(s, "SIOCGIWNAME nosuchif0", SIOCGIWNAME, "nosuchif0", 0);
    iw(s, "SIOCGIWPRIV+0x1f (private range) lo", SIOCIWFIRSTPRIV + 0x1f, "lo", 0);
    /* wext_handle_ioctl copies the iwreq in before anything else (wext-core.c:1063-1080). */
    iw(s, "SIOCGIWNAME kernel pointer", SIOCGIWNAME, "lo", KERNEL_ADDR);
    fflush(stdout);
    /* SET commands (even numbers) and SIOCGIWENCODE need CAP_NET_ADMIN before the device lookup (:990-998). */
    pid_t c = fork();
    if (c == 0) {
        if (geteuid() == 0 && (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534)))
            _exit(3);
        iw(s, "unprivileged SIOCSIWESSID lo", SIOCSIWESSID, "lo", 0);
        iw(s, "unprivileged SIOCSIWESSID nosuchif0", SIOCSIWESSID, "nosuchif0", 0);
        iw(s, "unprivileged SIOCGIWENCODE lo", SIOCGIWENCODE, "lo", 0);
        iw(s, "unprivileged SIOCGIWNAME nosuchif0", SIOCGIWNAME, "nosuchif0", 0);
        fflush(stdout);
        _exit(0);
    }
    int st = 0;
    waitpid(c, &st, 0);
    printf("unprivileged child: exit %d\n", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
    close(s);
}

static int uevent_sock(void) {
    return socket(AF_NETLINK, SOCK_DGRAM | SOCK_CLOEXEC | SOCK_NONBLOCK, NETLINK_KOBJECT_UEVENT);
}

/* One socket per bind: a second bind with nl_pid 0 on a bound socket is EINVAL (af_netlink.c netlink_bind). */
static void bind_group(const char *label, unsigned groups) {
    int s = uevent_sock();
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = groups};
    printf("%s = %s\n", label, bind(s, (struct sockaddr *)&a, sizeof a) == 0 ? "0" : sename(errno));
    close(s);
}

static void uevent(void) {
    int s = uevent_sock();
    if (s < 0) {
        printf("socket = -%s\n", sename(errno));
        return;
    }
    printf("socket = ok\n");
    close(s);
    bind_group("bind group 1 (kernel)", 1);
    bind_group("bind group 2 (udev)", 2);
    bind_group("bind groups 1|2", 3);
    bind_group("bind all 32 groups", 0xffffffffu);
    /* As libudev's udev_monitor_enable_receiving: filter, bind, getsockname for the port id, SO_PASSCRED. */
    s = uevent_sock();
    struct sock_filter ins[] = {BPF_STMT(BPF_RET | BPF_K, 0xffffffffu)};
    struct sock_fprog prog = {.len = 1, .filter = ins};
    R("SO_ATTACH_FILTER", setsockopt(s, SOL_SOCKET, SO_ATTACH_FILTER, &prog, sizeof prog));
    struct sockaddr_nl a = {.nl_family = AF_NETLINK, .nl_groups = 2};
    R("bind group 2", bind(s, (struct sockaddr *)&a, sizeof a));
    struct sockaddr_nl got;
    memset(&got, 0xaa, sizeof got);
    socklen_t len = sizeof got;
    R("getsockname", getsockname(s, (struct sockaddr *)&got, &len));
    printf("getsockname: len %u family %d groups %#x\n", (unsigned)len, got.nl_family, got.nl_groups);
    yes("getsockname: a port id was given", got.nl_pid != 0);
    int one = 1;
    R("SO_PASSCRED", setsockopt(s, SOL_SOCKET, SO_PASSCRED, &one, sizeof one));
    int rcv = 1 << 20;
    R("SO_RCVBUF 1 MiB", setsockopt(s, SOL_SOCKET, SO_RCVBUF, &rcv, sizeof rcv));
    char buf[64];
    R("recv with nothing sent", recv(s, buf, sizeof buf, MSG_DONTWAIT));
    close(s);
}

int main(int argc, char **argv) {
    const char *p = argc > 1 ? argv[1] : "";
    if (!strcmp(p, "tasknlink"))
        tasknlink();
    else if (!strcmp(p, "siocgiw"))
        siocgiw();
    else if (!strcmp(p, "procnlink"))
        procnlink();
    else if (!strcmp(p, "uevent"))
        uevent();
    else
        printf("usage: abi-br3 tasknlink|procnlink|siocgiw|uevent\n");
    return 0;
}
