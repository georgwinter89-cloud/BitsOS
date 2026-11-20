/* DB2: touches exactly what the access log must show (ktest access.hooks.probe, scenario ab/db2-werhatwas):
 * reads <infile>, makes <dir> if needed, makes, renames and removes a file and a folder in it, listens on
 * 127.0.0.1, connects to itself and sends "hallo" (5 bytes) from the connecting side, and opens the first of the
 * comma-separated <devices> that opens ("-": none). Prints "port <n>", "device <path>" and "done" (or
 * "fail <step>" and exit 1). */
#include <errno.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

static int fail(const char *step) {
    printf("fail %s\n", step);
    return 1;
}

int main(int argc, char **argv) {
    if (argc < 4) return fail("usage: access-probe INFILE DIR DEVICES");
    char buf[64], a[512], b[512];
    int fd = open(argv[1], O_RDONLY);
    if (fd < 0 || read(fd, buf, sizeof buf) < 0) return fail("read");
    close(fd);
    if (mkdir(argv[2], 0755) != 0 && errno != EEXIST) return fail("dir");
    snprintf(a, sizeof a, "%s/neu.txt", argv[2]);
    snprintf(b, sizeof b, "%s/umbenannt.txt", argv[2]);
    fd = open(a, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (fd < 0 || write(fd, "x", 1) != 1) return fail("create");
    close(fd);
    if (rename(a, b) != 0) return fail("rename");
    if (unlink(b) != 0) return fail("unlink");
    snprintf(a, sizeof a, "%s/ordner", argv[2]);
    if (mkdir(a, 0755) != 0 || rmdir(a) != 0) return fail("mkdir");

    int l = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in sa;
    memset(&sa, 0, sizeof sa);
    sa.sin_family = AF_INET;
    sa.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    socklen_t len = sizeof sa;
    if (l < 0 || bind(l, (struct sockaddr *)&sa, sizeof sa) != 0 || listen(l, 4) != 0) return fail("listen");
    if (getsockname(l, (struct sockaddr *)&sa, &len) != 0) return fail("getsockname");
    printf("port %d\n", ntohs(sa.sin_port));
    int c = socket(AF_INET, SOCK_STREAM, 0);
    if (c < 0 || connect(c, (struct sockaddr *)&sa, sizeof sa) != 0) return fail("connect");
    int s = accept(l, 0, 0);
    if (s < 0) return fail("accept");
    if (send(c, "hallo", 5, 0) != 5) return fail("send");
    int got = 0;
    while (got < 5) {
        int n = recv(s, buf + got, 5 - got, 0);
        if (n <= 0) return fail("recv");
        got += n;
    }
    close(c);
    close(s);
    close(l);

    if (strcmp(argv[3], "-") != 0) {
        char list[512];
        snprintf(list, sizeof list, "%s", argv[3]);
        fd = -1;
        for (char *d = strtok(list, ","); d && fd < 0; d = strtok(0, ",")) {
            fd = open(d, O_RDONLY | O_NONBLOCK);
            if (fd >= 0) printf("device %s\n", d);
        }
        if (fd < 0) return fail("device");
        close(fd);
    }
    printf("done\n");
    fflush(stdout);
    return 0;
}
