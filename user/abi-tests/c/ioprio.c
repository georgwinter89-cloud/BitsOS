/* FS-12: ioprio_set/ioprio_get (linux-6.6/block/ioprio.c). No RT class (it depends on who runs the test), no
 * IOPRIO_WHO_USER (other processes of the user decide it); PGRP only for a child in its own group. */
#include "abi.h"
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#define CLASS_SHIFT 13
#define VALUE(c, d) (((c) << CLASS_SHIFT) | (d))
#define WHO_PROCESS 1
#define WHO_PGRP 2

int main(void) {
    SYS("get.default", SYS_ioprio_get, WHO_PROCESS, 0);
    SYS("get.which0", SYS_ioprio_get, 0, 0);
    SYS("get.which4", SYS_ioprio_get, 4, 0);
    SYS("get.nopid", SYS_ioprio_get, WHO_PROCESS, 0x7fffffff);
    SYS("set.which0", SYS_ioprio_set, 0, 0, VALUE(3, 0));
    SYS("set.none_level4", SYS_ioprio_set, WHO_PROCESS, 0, VALUE(0, 4));
    SYS("set.class4", SYS_ioprio_set, WHO_PROCESS, 0, VALUE(4, 0));
    SYS("set.invalid_before_which", SYS_ioprio_set, 9, 0, -1);
    SYS("set.nopid", SYS_ioprio_set, WHO_PROCESS, 0x7fffffff, VALUE(3, 0));
    SYS("set.pid_minus1", SYS_ioprio_set, WHO_PROCESS, -1, VALUE(3, 0));
    SYS("set.be7", SYS_ioprio_set, WHO_PROCESS, 0, VALUE(2, 7));
    SYS("get.be7", SYS_ioprio_get, WHO_PROCESS, 0);
    SYS("set.be8_hint_bits", SYS_ioprio_set, WHO_PROCESS, 0, VALUE(2, 8));
    SYS("get.be8", SYS_ioprio_get, WHO_PROCESS, 0);
    SYS("set.idle", SYS_ioprio_set, WHO_PROCESS, 0, VALUE(3, 0));
    SYS("get.idle", SYS_ioprio_get, WHO_PROCESS, 0);
    SYS("set.self_by_pid", SYS_ioprio_set, WHO_PROCESS, getpid(), VALUE(3, 5));
    SYS("get.self_by_pid", SYS_ioprio_get, WHO_PROCESS, getpid());
    fflush(stdout);
    pid_t p = fork();
    if (p == 0) {
        /* The child inherits the value (`__copy_io`); in its own group PGRP gives it too. */
        SYS("child.get", SYS_ioprio_get, WHO_PROCESS, 0);
        setpgid(0, 0);
        SYS("child.get_pgrp", SYS_ioprio_get, WHO_PGRP, 0);
        SYS("child.set_none", SYS_ioprio_set, WHO_PROCESS, 0, 0);
        SYS("child.get_none", SYS_ioprio_get, WHO_PROCESS, 0);
        /* NONE: PGRP reports the effective value, BE with level (nice + 20) / 5 (nice set to a known 10). */
        SYS("child.nice10", SYS_setpriority, PRIO_PROCESS, 0, 10);
        SYS("child.get_pgrp_derived", SYS_ioprio_get, WHO_PGRP, 0);
        fflush(stdout);
        _exit(0);
    }
    int st = 0;
    waitpid(p, &st, 0);
    yes("child.exit0", WIFEXITED(st) && WEXITSTATUS(st) == 0);
    SYS("get.parent_unchanged", SYS_ioprio_get, WHO_PROCESS, 0);
    SYS("set.none", SYS_ioprio_set, WHO_PROCESS, 0, 0);
    SYS("get.none", SYS_ioprio_get, WHO_PROCESS, 0);
    return 0;
}
