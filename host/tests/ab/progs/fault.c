/* AB-2: prints a line, then writes to address 0. Linux ends it with SIGSEGV; V2 the same, with its message. */
#include <stdio.h>
int main(void) {
  printf("about to fault\n");
  fflush(stdout);
  *(volatile int *)0 = 1;
  return 0;
}
