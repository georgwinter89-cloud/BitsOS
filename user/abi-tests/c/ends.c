/* Wave A: how programs end. argv[1] picks the way: exit codes, exit_group's low byte, abort() (musl's raise
 * with all signals blocked, then the default action on unblock), a blocked SIGTERM that ends the program
 * when unblocked, and SIGKILL to itself. */
#include "abi.h"
#include <signal.h>

int main(int argc, char **argv) {
    const char *how = argc > 1 ? argv[1] : "exit0";
    printf("way: %s\n", how);
    fflush(stdout);
    if (!strcmp(how, "exit0"))
        return 0;
    if (!strcmp(how, "exit7"))
        exit(7);
    if (!strcmp(how, "exit_group300"))
        syscall(SYS_exit_group, 300);
    if (!strcmp(how, "abort"))
        abort();
    if (!strcmp(how, "raise-blocked")) {
        sigset_t s;
        sigemptyset(&s);
        sigaddset(&s, SIGTERM);
        sigprocmask(SIG_BLOCK, &s, 0);
        raise(SIGTERM);
        puts("alive with SIGTERM pending");
        fflush(stdout);
        sigprocmask(SIG_UNBLOCK, &s, 0);
        puts("not reached");
        return 0;
    }
    if (!strcmp(how, "sigkill")) {
        syscall(SYS_kill, getpid(), SIGKILL);
        puts("not reached");
        return 0;
    }
    if (!strcmp(how, "stderr")) {
        fprintf(stderr, "a line on stderr\n");
        printf("no newline at the end");
        return 3;
    }
    return 99;
}
