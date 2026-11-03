/* SX-3b: findmnt for bubblewrap's test suite on V2 (util-linux is not ported; the suite only greps the list):
 * TARGET SOURCE FSTYPE OPTIONS of every line of /proc/self/mountinfo, flat (no tree, no options). */
#include <stdio.h>
#include <string.h>

int main(void)
{
  FILE *f = fopen("/proc/self/mountinfo", "r");
  if (!f)
    {
      perror("findmnt: /proc/self/mountinfo");
      return 1;
    }
  char line[8192];
  printf("TARGET SOURCE FSTYPE OPTIONS\n");
  while (fgets(line, sizeof line, f))
    {
      char *field[12] = { 0 };
      int n = 0;
      for (char *s = strtok(line, " \n"); s && n < 12; s = strtok(NULL, " \n"))
        field[n++] = s;
      int dash = 6;
      while (dash < n && strcmp(field[dash], "-") != 0)
        dash++;
      if (n < 5 || dash + 3 > n)
        continue;
      printf("%s %s %s %s\n", field[4], field[dash + 2], field[dash + 1], field[5]);
    }
  fclose(f);
  return 0;
}
