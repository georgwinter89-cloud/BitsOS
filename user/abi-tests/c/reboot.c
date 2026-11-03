/* UL-7: reboot(2)'s refusals (linux-6.6/kernel/reboot.c :700). Never a valid command: every call carries a
 * command no kernel knows, so even a caller with CAP_SYS_BOOT gets EINVAL and nothing restarts. Without the
 * capability (the ABI diff runs as an ordinary user on CachyOS and as MCP's user 1000 on V2) EPERM comes first,
 * before the magic numbers are looked at. */
#include "abi.h"

#define MAGIC1 0xfee1deadu
#define MAGIC2 672274793u
#define BOGUS 0x12345678u

int main(void) {
    SYS("reboot.bad_magic1", SYS_reboot, 0u, MAGIC2, BOGUS, 0);
    SYS("reboot.bad_magic2", SYS_reboot, MAGIC1, 1u, BOGUS, 0);
    SYS("reboot.unknown_cmd", SYS_reboot, MAGIC1, MAGIC2, BOGUS, 0);
    SYS("reboot.unknown_cmd_bad_arg", SYS_reboot, MAGIC1, MAGIC2, BOGUS, (long)KERNEL_ADDR);
    return 0;
}
