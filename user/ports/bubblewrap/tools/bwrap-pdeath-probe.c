/* SX-3b fixer: does a pid namespace's init whose parent is outside the namespace die by its PR_SET_PDEATHSIG
 * when that parent ends? bubblewrap's "--die-with-parent --unshare-pid" check needs it (linux-6.6/kernel/
 * signal.c:1233: the signal from a sender invisible in the init's namespace is forced). Without it the suite
 * would hang in its lock wait, so patches/test-run-lockf.patch skips that one check when this answers no.
 * Exit 0 = yes, 1 = no (the init is killed here then), 2 = the probe itself could not run. 5 s bound. */
#define _GNU_SOURCE
#include <sched.h>
#include <signal.h>
#include <sys/prctl.h>
#include <sys/wait.h>
#include <unistd.h>

int main(void)
{
  int ready[2], idp[2];
  if (prctl(PR_SET_CHILD_SUBREAPER, 1) || pipe(ready) || pipe(idp))
    return 2;
  pid_t b = fork();
  if (b < 0)
    return 2;
  if (b == 0) {
    if (unshare(CLONE_NEWUSER | CLONE_NEWPID))
      _exit(2);
    pid_t c = fork();
    if (c == 0) {
      char one = 1;
      if (prctl(PR_SET_PDEATHSIG, SIGKILL) || write(ready[1], &one, 1) != 1)
        _exit(2);
      for (;;)
        pause();
    }
    char got;
    if (c < 0 || read(ready[0], &got, 1) != 1 || write(idp[1], &c, sizeof c) != sizeof c)
      _exit(2);
    _exit(0);
  }
  close(ready[1]);
  close(idp[1]);
  pid_t c = 0;
  int st = 0;
  if (read(idp[0], &c, sizeof c) != sizeof c)
    c = 0;
  waitpid(b, &st, 0);
  if (c <= 0 || !WIFEXITED(st) || WEXITSTATUS(st) != 0)
    return 2;
  for (int i = 0; i < 50; i++) {
    pid_t r = waitpid(c, &st, WNOHANG);
    if (r == c)
      return WIFSIGNALED(st) && WTERMSIG(st) == SIGKILL ? 0 : 1;
    if (r < 0)
      return 2;
    usleep(100000);
  }
  kill(c, SIGKILL);
  waitpid(c, &st, 0);
  return 1;
}
