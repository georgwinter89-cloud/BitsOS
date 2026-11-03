/* SX-3b: the one GNU df form bubblewrap's test suite uses ("tmpfs has expected size"), which BusyBox's df lacks:
 * df --output=size --block-size=1K PATH. statfs(2) of PATH, printed as GNU coreutils does: the header
 * "1K-blocks" and the size in KiB, right-aligned to the header. Any other form: exit 1 (as an unknown option). */
#include <stdio.h>
#include <string.h>
#include <sys/statfs.h>

int main(int argc, char **argv)
{
  const char *path = NULL;
  int size = 0, kib = 0;
  for (int i = 1; i < argc; i++)
    {
      if (strcmp(argv[i], "--output=size") == 0)
        size = 1;
      else if (strcmp(argv[i], "--block-size=1K") == 0)
        kib = 1;
      else if (argv[i][0] != '-' && !path)
        path = argv[i];
      else
        {
          fprintf(stderr, "df: unsupported argument '%s' (V2 stand-in: --output=size --block-size=1K PATH)\n", argv[i]);
          return 1;
        }
    }
  if (!size || !kib || !path)
    {
      fprintf(stderr, "df: V2 stand-in needs --output=size --block-size=1K PATH\n");
      return 1;
    }
  struct statfs s;
  if (statfs(path, &s) != 0)
    {
      perror(path);
      return 1;
    }
  unsigned long long k = (unsigned long long) s.f_blocks * (unsigned long long) s.f_bsize / 1024;
  printf("1K-blocks\n%9llu\n", k);
  return 0;
}
