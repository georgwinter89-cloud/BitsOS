/* Wave A: ioctl. Terminal ioctls on files that are not terminals answer ENOTTY (the harness runs programs
 * with pipes, so the console's TCGETS/TIOCGWINSZ are a V2 ktest: ab.wave_a.ioctl_console); the generic
 * ioctls every file has (FIOCLEX, FIONCLEX, FIONBIO, FIONREAD on a regular file). */
#include "abi.h"
#include <fcntl.h>
#include <sys/ioctl.h>
#include <termios.h>

int main(void) {
    struct termios t;
    struct winsize w;
    int null = open("/dev/null", O_RDWR);
    char path[64];
    snprintf(path, sizeof path, "/tmp/abi-ioctl-%d", (int)getpid());
    int file = open(path, O_RDWR | O_CREAT | O_TRUNC, 0600);
    SYS("ioctl(stdin pipe, TCGETS)", SYS_ioctl, 0, TCGETS, &t);
    SYS("ioctl(stdout pipe, TIOCGWINSZ)", SYS_ioctl, 1, TIOCGWINSZ, &w);
    SYS("ioctl(/dev/null, TCGETS)", SYS_ioctl, null, TCGETS, &t);
    SYS("ioctl(/dev/null, TIOCGWINSZ)", SYS_ioctl, null, TIOCGWINSZ, &w);
    SYS("ioctl(file, TCGETS)", SYS_ioctl, file, TCGETS, &t);
    SYS("ioctl(closed fd, TCGETS)", SYS_ioctl, 77, TCGETS, &t);
    SYS("ioctl(-1, TCGETS)", SYS_ioctl, -1, TCGETS, &t);
    for (int fd = 0; fd < 3; fd++) {
        errno = 0;
        int r = isatty(fd);
        printf("isatty(%d) = %d %s\n", fd, r, r ? "" : ename(errno));
    }
    SYS("ioctl(file, FIOCLEX)", SYS_ioctl, file, FIOCLEX, 0);
    printf("close-on-exec after FIOCLEX: %d\n", fcntl(file, F_GETFD) & FD_CLOEXEC);
    SYS("ioctl(file, FIONCLEX)", SYS_ioctl, file, FIONCLEX, 0);
    printf("close-on-exec after FIONCLEX: %d\n", fcntl(file, F_GETFD) & FD_CLOEXEC);
    int on = 1;
    SYS("ioctl(file, FIONBIO, 1)", SYS_ioctl, file, FIONBIO, &on);
    printf("O_NONBLOCK after FIONBIO 1: %d\n", !!(fcntl(file, F_GETFL) & O_NONBLOCK));
    on = 0;
    SYS("ioctl(file, FIONBIO, 0)", SYS_ioctl, file, FIONBIO, &on);
    printf("O_NONBLOCK after FIONBIO 0: %d\n", !!(fcntl(file, F_GETFL) & O_NONBLOCK));
    SYS("ioctl(file, FIONBIO, bad pointer)", SYS_ioctl, file, FIONBIO, KERNEL_ADDR);
    if (write(file, "0123456789", 10) != 10)
        puts("write failed");
    lseek(file, 3, SEEK_SET);
    int n = -1;
    SYS("ioctl(file, FIONREAD)", SYS_ioctl, file, FIONREAD, &n);
    printf("FIONREAD after seeking to 3: %d\n", n);
    SYS("ioctl(file, FIONREAD, bad pointer)", SYS_ioctl, file, FIONREAD, KERNEL_ADDR);
    SYS("ioctl(file, 0x12345678)", SYS_ioctl, file, 0x12345678, 0);
    close(file);
    unlink(path);
    close(null);
    return 0;
}
