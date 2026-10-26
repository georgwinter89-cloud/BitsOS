/* G0 ring-3 smoke test: static musl C startup (kernel ktest smoke.hello_c checks the line). */
#include <stdio.h>
int main(void) { printf("hello from musl C on BitsOS V2\n"); return 0; }
