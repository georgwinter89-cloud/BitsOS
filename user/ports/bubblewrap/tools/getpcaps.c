/* SX-3b: getpcaps for bubblewrap's test suite on V2 (libcap's tool is not ported). capget(2) of each pid, printed
 * the way libcap's cap_to_text does for the common cases: "<pid>: =" with no capabilities, "=ep" when every
 * capability has the same flags, otherwise the most common flags first and the others as "name+x" / "name-x". */
#include <linux/capability.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>

static const char *const names[] = {
  "chown", "dac_override", "dac_read_search", "fowner", "fsetid", "kill", "setgid", "setuid", "setpcap",
  "linux_immutable", "net_bind_service", "net_broadcast", "net_admin", "net_raw", "ipc_lock", "ipc_owner",
  "sys_module", "sys_rawio", "sys_chroot", "sys_ptrace", "sys_pacct", "sys_admin", "sys_boot", "sys_nice",
  "sys_resource", "sys_time", "sys_tty_config", "mknod", "lease", "audit_write", "audit_control", "setfcap",
  "mac_override", "mac_admin", "syslog", "wake_alarm", "block_suspend", "audit_read", "perfmon", "bpf",
  "checkpoint_restore",
};
#define NCAP ((int) (sizeof names / sizeof names[0]))

static void flags(char *buf, int f)
{
  char *p = buf;
  if (f & 1) *p++ = 'e';
  if (f & 2) *p++ = 'i';
  if (f & 4) *p++ = 'p';
  *p = 0;
}

static int show(const char *arg)
{
  struct __user_cap_header_struct h = { _LINUX_CAPABILITY_VERSION_3, atoi(arg) };
  struct __user_cap_data_struct d[2];
  memset(d, 0, sizeof d);
  if (syscall(SYS_capget, &h, d) < 0)
    {
      perror("capget");
      return 1;
    }
  int f[NCAP], count[8] = { 0 };
  for (int c = 0; c < NCAP; c++)
    {
      unsigned bit = 1u << (c % 32);
      int i = c / 32;
      f[c] = (d[i].effective & bit ? 1 : 0) | (d[i].inheritable & bit ? 2 : 0) | (d[i].permitted & bit ? 4 : 0);
      count[f[c]]++;
    }
  int base = 0;
  for (int k = 1; k < 8; k++)
    if (count[k] > count[base])
      base = k;
  char b[4];
  flags(b, base);
  printf("%s: %s%s", arg, base ? "=" : count[0] == NCAP ? "=" : "", b);
  int first = !base;
  for (int k = 0; k < 8; k++)
    {
      if (k == base || count[k] == 0)
        continue;
      if (!first)
        putchar(' ');
      first = 0;
      int sep = 0;
      for (int c = 0; c < NCAP; c++)
        if (f[c] == k)
          printf("%scap_%s", sep++ ? "," : "", names[c]);
      char add[4], del[4];
      flags(add, k & ~base);
      flags(del, base & ~k);
      if (!base)
        printf("=%s", add);
      else
        printf("%s%s%s%s", *add ? "+" : "", add, *del ? "-" : "", del);
    }
  printf("\n");
  return 0;
}

int main(int argc, char **argv)
{
  int rc = 0;
  for (int i = 1; i < argc; i++)
    rc |= show(argv[i]);
  return rc;
}
