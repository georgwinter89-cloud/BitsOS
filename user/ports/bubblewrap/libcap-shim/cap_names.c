/* cap_from_name for the bubblewrap port (see sys/capability.h). Names as the kernel's CAP_* constants. */
#include <stddef.h>
#include <strings.h>
#include <sys/capability.h>

static const char *const names[] = {
  "chown", "dac_override", "dac_read_search", "fowner", "fsetid", "kill", "setgid", "setuid", "setpcap",
  "linux_immutable", "net_bind_service", "net_broadcast", "net_admin", "net_raw", "ipc_lock", "ipc_owner",
  "sys_module", "sys_rawio", "sys_chroot", "sys_ptrace", "sys_pacct", "sys_admin", "sys_boot", "sys_nice",
  "sys_resource", "sys_time", "sys_tty_config", "mknod", "lease", "audit_write", "audit_control", "setfcap",
  "mac_override", "mac_admin", "syslog", "wake_alarm", "block_suspend", "audit_read", "perfmon", "bpf",
  "checkpoint_restore",
};

int cap_from_name(const char *name, cap_value_t *value)
{
  if (name == NULL || value == NULL)
    return -1;
  if (*name >= '0' && *name <= '9')
    {
      long n = 0;
      for (const char *p = name; *p; p++)
        {
          if (*p < '0' || *p > '9')
            return -1;
          n = n * 10 + (*p - '0');
          if (n > CAP_LAST_CAP)
            return -1;
        }
      *value = (cap_value_t) n;
      return 0;
    }
  if (strncasecmp (name, "cap_", 4) != 0)
    return -1;
  for (size_t i = 0; i < sizeof names / sizeof names[0] && i <= CAP_LAST_CAP; i++)
    if (strcasecmp (name + 4, names[i]) == 0)
      {
        *value = (cap_value_t) i;
        return 0;
      }
  return -1;
}
