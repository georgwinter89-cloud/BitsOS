// BR-9: a fake Brave for pk/97-browser.sh (never the real one in QEMU). Installed as both /opt/brave-bin/brave (the
// binary) and /usr/bin/brave (the vendor's start script, which must not run). Prints `key = value` lines:
//   its arguments and environment; the profile (Chromium's default under XDG_CONFIG_HOME) and what the stick's
//   profile brought; Chromium's namespace sandbox calls (a new user + network namespace with the caller's id mapped
//   and setgroups denied, then a PID namespace with a child as pid 1 that chroots to /proc/self/fdinfo: the calls of
//   sandbox/linux/services/namespace_sandbox.cc and credentials.cc); Chromium's singleton: the first start (`--hold`)
//   listens on a socket in a fresh folder under TMPDIR with the profile's SingletonSocket link to it, a second start
//   finds the link and hands its arguments over (process_singleton_posix.cc).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <unistd.h>

static const char *env(const char *k) {
    const char *v = getenv(k);
    return v ? v : "(unset)";
}

static int put(const char *path, const char *text) {
    int fd = open(path, O_WRONLY | O_CREAT | O_TRUNC, 0600);
    if (fd < 0) return -1;
    ssize_t n = write(fd, text, strlen(text));
    close(fd);
    return n == (ssize_t)strlen(text) ? 0 : -1;
}

static void show(const char *key, const char *path) {
    char buf[256] = {0};
    int fd = open(path, O_RDONLY);
    ssize_t n = fd < 0 ? -1 : read(fd, buf, sizeof buf - 1);
    if (fd >= 0) close(fd);
    printf("%s = %s\n", key, n < 0 ? strerror(errno) : buf);
}

// The child side of Chromium's namespace sandbox; returns 0 when every call worked, else prints the failed step.
static int sandbox(void) {
    char outer[64] = {0}, inner[64] = {0};
    if (readlink("/proc/self/ns/user", outer, sizeof outer - 1) < 0) return printf("sandbox = FAIL readlink outer %s\n", strerror(errno)), 1;
    uid_t uid = getuid();
    gid_t gid = getgid();
    if (unshare(CLONE_NEWUSER | CLONE_NEWNET) != 0) return printf("sandbox = FAIL unshare user+net %s\n", strerror(errno)), 1;
    char map[64];
    if (put("/proc/self/setgroups", "deny") != 0) return printf("sandbox = FAIL setgroups %s\n", strerror(errno)), 1;
    snprintf(map, sizeof map, "%u %u 1\n", uid, uid);
    if (put("/proc/self/uid_map", map) != 0) return printf("sandbox = FAIL uid_map %s\n", strerror(errno)), 1;
    snprintf(map, sizeof map, "%u %u 1\n", gid, gid);
    if (put("/proc/self/gid_map", map) != 0) return printf("sandbox = FAIL gid_map %s\n", strerror(errno)), 1;
    if (readlink("/proc/self/ns/user", inner, sizeof inner - 1) < 0 || strcmp(inner, outer) == 0) return printf("sandbox = FAIL no new user namespace\n"), 1;
    if (unshare(CLONE_NEWPID) != 0) return printf("sandbox = FAIL unshare pid %s\n", strerror(errno)), 1;
    fflush(stdout);
    pid_t p = fork();
    if (p < 0) return printf("sandbox = FAIL fork %s\n", strerror(errno)), 1;
    if (p == 0) {
        if (getpid() != 1) _exit(10);
        if (chroot("/proc/self/fdinfo") != 0 || chdir("/") != 0) _exit(11);
        _exit(0);
    }
    int st = 0;
    if (waitpid(p, &st, 0) != p || !WIFEXITED(st)) return printf("sandbox = FAIL wait\n"), 1;
    if (WEXITSTATUS(st) != 0) return printf("sandbox = FAIL child %s\n", WEXITSTATUS(st) == 10 ? "is not pid 1" : "chroot"), 1;
    printf("sandbox = ok (new user namespace %s inside %s, network and PID namespace, pid 1, chroot)\n", inner, outer);
    return 0;
}

int main(int argc, char **argv) {
    char self[256] = {0};
    if (readlink("/proc/self/exe", self, sizeof self - 1) < 0) strcpy(self, "?");
    printf("exe = %s\n", self);
    if (strcmp(self, "/usr/bin/brave") == 0) {
        printf("wrapper = ran (the start script, not the binary)\n");
        return 3;
    }
    printf("uid = %u\n", getuid());
    printf("args =");
    for (int i = 1; i < argc; i++) printf(" %s", argv[i]);
    printf("\n");
    const char *keys[] = {"TMPDIR", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "HOME", "LANG", "GTK_USE_PORTAL", "NO_AT_BRIDGE",
                          "DBUS_SESSION_BUS_ADDRESS", "PULSE_SERVER", "WAYLAND_DISPLAY", "LD_LIBRARY_PATH",
                          "AT_SPI_BUS_ADDRESS", "ACCESSIBILITY_ENABLED"};
    for (size_t i = 0; i < sizeof keys / sizeof keys[0]; i++) printf("env %s = %s\n", keys[i], env(keys[i]));
    int hold = argc > 1 && strcmp(argv[argc - 1], "--hold") == 0;
    char prof[512], f[600];
    snprintf(prof, sizeof prof, "%s/BraveSoftware/Brave-Browser", env("XDG_CONFIG_HOME"));
    snprintf(f, sizeof f, "%s/Default/Bookmarks", prof);
    show("bookmarks", f);
    snprintf(f, sizeof f, "%s/SingletonSocket", prof);
    char link[256] = {0};
    if (!hold && readlink(f, link, sizeof link - 1) > 0) {
        // A second start: hand the arguments to the running one, as Chromium's NotifyOtherProcess.
        int s = socket(AF_UNIX, SOCK_STREAM, 0);
        struct sockaddr_un a = {.sun_family = AF_UNIX};
        if (snprintf(a.sun_path, sizeof a.sun_path, "%s", link) >= (int)sizeof a.sun_path) return printf("handover = FAIL path too long\n"), 4;
        if (s < 0 || connect(s, (struct sockaddr *)&a, sizeof a) != 0) {
            printf("handover = FAIL connect %s: %s\n", link, strerror(errno));
            return 4;
        }
        char msg[512] = "START";
        for (int i = 1; i < argc; i++) snprintf(msg + strlen(msg), sizeof msg - strlen(msg), " %s", argv[i]);
        ssize_t n = write(s, msg, strlen(msg));
        close(s);
        printf("handover = %s\n", n > 0 ? "sent" : "FAIL write");
        return n > 0 ? 0 : 4;
    }
    // Chromium makes its folders itself.
    for (char *q = prof + 1; *q; q++)
        if (*q == '/') {
            *q = 0;
            mkdir(prof, 0700);
            *q = '/';
        }
    mkdir(prof, 0700);
    snprintf(f, sizeof f, "%s/Local State", prof);
    if (access(f, F_OK) != 0 && put(f, "{\"probe\":1}") != 0) {
        printf("profile = FAIL %s: %s\n", f, strerror(errno));
        return 5;
    }
    printf("profile = %s\n", prof);
    // In a child, as Chromium's zygote: the browser process itself stays in the view's namespaces.
    fflush(stdout);
    pid_t sp = fork();
    if (sp == 0) {
        int r = sandbox();
        fflush(stdout);
        _exit(r);
    }
    int sst = 0;
    int rc = sp > 0 && waitpid(sp, &sst, 0) == sp && WIFEXITED(sst) ? WEXITSTATUS(sst) : 1;
    if (!hold) return rc;
    char dir[512];
    snprintf(dir, sizeof dir, "%s/.org.chromium.Chromium.XXXXXX", env("TMPDIR"));
    if (!mkdtemp(dir)) return printf("singleton = FAIL mkdtemp %s\n", strerror(errno)), 6;
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    if (snprintf(a.sun_path, sizeof a.sun_path, "%s/SingletonSocket", dir) >= (int)sizeof a.sun_path) return printf("singleton = FAIL path too long\n"), 6;
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) != 0 || listen(l, 1) != 0) return printf("singleton = FAIL listen %s\n", strerror(errno)), 6;
    snprintf(f, sizeof f, "%s/SingletonSocket", prof);
    unlink(f);
    if (symlink(a.sun_path, f) != 0) return printf("singleton = FAIL link %s\n", strerror(errno)), 6;
    printf("singleton = listening %s\n", a.sun_path);
    fflush(stdout);
    alarm(60);
    int c = accept(l, NULL, NULL);
    char buf[512] = {0};
    ssize_t n = c < 0 ? -1 : read(c, buf, sizeof buf - 1);
    printf("received = %s\n", n > 0 ? buf : "nothing");
    unlink(f);
    unlink(a.sun_path);
    rmdir(dir);
    return rc != 0 ? rc : n > 0 ? 0 : 7;
}
