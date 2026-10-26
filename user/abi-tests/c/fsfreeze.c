/* VS-2 CR5: FIFREEZE / FITHAW (linux-6.6/fs/ioctl.c:387-415) as an unprivileged program: the capability check
 * comes first, so every file answers EPERM, a closed fd EBADF. The root answers (EOPNOTSUPP on tmpfs, EBUSY,
 * EINVAL, a real freeze of an ext4 /data) are the V2 ktest ab.fsfreeze, since the diff runs as a user on
 * CachyOS. As root (V2) the program drops to 65534 first. */
#include "abi.h"
#include <fcntl.h>
#include <sys/ioctl.h>

#ifndef FIFREEZE
#define FIFREEZE 0xC0045877
#define FITHAW 0xC0045878
#endif

int main(void) {
    if (geteuid() == 0 && (setresgid(65534, 65534, 65534) || setresuid(65534, 65534, 65534))) {
        puts("could not drop root");
        return 1;
    }
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-fsfreeze-%d", (int)getpid());
    int file = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    int root = open("/", O_RDONLY | O_DIRECTORY);
    int p[2];
    if (file < 0 || root < 0 || pipe(p)) {
        printf("setup failed: %s\n", ename(errno));
        return 1;
    }
    SYS("ioctl(tmp file, FIFREEZE)", SYS_ioctl, file, FIFREEZE, 0);
    SYS("ioctl(tmp file, FITHAW)", SYS_ioctl, file, FITHAW, 0);
    SYS("ioctl(/, FIFREEZE)", SYS_ioctl, root, FIFREEZE, 0);
    SYS("ioctl(/, FITHAW)", SYS_ioctl, root, FITHAW, 0);
    SYS("ioctl(pipe, FIFREEZE)", SYS_ioctl, p[0], FIFREEZE, 0);
    SYS("ioctl(pipe, FITHAW)", SYS_ioctl, p[1], FITHAW, 0);
    SYS("ioctl(closed fd, FIFREEZE)", SYS_ioctl, 77, FIFREEZE, 0);
    SYS("ioctl(-1, FITHAW)", SYS_ioctl, -1, FITHAW, 0);
    unlink(path);
    return 0;
}
