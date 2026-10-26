#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv) {
  char *buf = malloc(64);
  if (!buf) return 1;
  snprintf(buf, 64, "hello from C on musl, argc=%d", argc);
  printf("%s, len=%zu, pid>0=%d\n", buf, strlen(buf), getpid() > 0);
  free(buf);
  return 0;
}
