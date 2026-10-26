/* AB-2: reads stdin to its end, prints it in upper case with the byte count, exits with the count % 256.
 * Checks the run tool's stdin and capture (both pipes, not terminals). */
#include <ctype.h>
#include <stdio.h>
#include <unistd.h>

int main(int argc, char **argv) {
  char buf[256];
  long total = 0;
  ssize_t n;
  printf("isatty(0)=%d isatty(1)=%d\n", isatty(0), isatty(1));
  while ((n = read(0, buf, sizeof buf)) > 0) {
    for (ssize_t i = 0; i < n; i++) buf[i] = toupper((unsigned char)buf[i]);
    fwrite(buf, 1, n, stdout);
    total += n;
  }
  printf("\nread %ld bytes, last read %zd\n", total, n);
  fprintf(stderr, "to stderr: %s\n", argc > 1 ? argv[1] : "-");
  return (int)(total % 256);
}
