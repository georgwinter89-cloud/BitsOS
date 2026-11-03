/* SX-3b: the lock probe of bubblewrap's "die with parent" check (test-run.sh writes it in Python; V2 has no
 * Python, patches/test-run-lockf.patch makes the script call this). Same calls: open for append (created 0666),
 * fcntl F_SETLK ("nowait") or F_SETLKW ("wait") with a write lock on the whole file. Exit 0 = locked, 1 = not. */
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char **argv)
{
  if (argc != 3)
    return 2;
  int fd = open(argv[1], O_WRONLY | O_CREAT | O_APPEND | O_CLOEXEC, 0666);
  if (fd < 0)
    return 1;
  struct flock l;
  memset(&l, 0, sizeof l);
  l.l_type = F_WRLCK;
  l.l_whence = SEEK_SET;
  int cmd = strcmp(argv[2], "wait") == 0 ? F_SETLKW : F_SETLK;
  return fcntl(fd, cmd, &l) < 0 ? 1 : 0;
}
