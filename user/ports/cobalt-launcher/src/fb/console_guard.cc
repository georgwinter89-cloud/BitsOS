// BitsOS V2 Cobalt backend: gives the text console back however Cobalt ends.

#include "starboard/shared/bitsos/fb/console_guard.h"

#include <linux/kd.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>

#include <atomic>

namespace bitsos_fb {
namespace {

struct Guarded {
  int sig;
  bool fatal;
};

const Guarded kSignals[] = {
    {SIGSEGV, true},  {SIGBUS, true},   {SIGFPE, true},
    {SIGILL, true},   {SIGABRT, true},  {SIGTERM, false},
    {SIGINT, false},  {SIGHUP, false},  {SIGQUIT, false},
};
constexpr int kCount = sizeof(kSignals) / sizeof(kSignals[0]);

std::atomic<int> g_fd{-1};
std::atomic<RestoreFn> g_restore{nullptr};
struct sigaction g_old[kCount];
bool g_installed = false;
bool g_atexit = false;

void RunRestore() {
  const int fd = g_fd.exchange(-1);
  const RestoreFn fn = g_restore.load();
  if (fd >= 0 && fn) {
    fn(fd);
  }
}

bool HasHandler(const struct sigaction& a) {
  return (a.sa_flags & SA_SIGINFO) ? a.sa_sigaction != nullptr
                                   : a.sa_handler != SIG_DFL &&
                                         a.sa_handler != SIG_IGN;
}

void Handler(int sig, siginfo_t* info, void* context) {
  for (int i = 0; i < kCount; ++i) {
    if (kSignals[i].sig != sig) {
      continue;
    }
    const struct sigaction& old = g_old[i];
    if (kSignals[i].fatal || !HasHandler(old)) {
      RunRestore();
    }
    if (HasHandler(old)) {
      if (old.sa_flags & SA_SIGINFO) {
        old.sa_sigaction(sig, info, context);
      } else {
        old.sa_handler(sig);
      }
      return;
    }
    // Default action (or ignore): put it back and let the signal act once
    // this handler returns (it is blocked until then).
    sigaction(sig, &old, nullptr);
    raise(sig);
    return;
  }
}

void AtExit() {
  RunRestore();
}

}  // namespace

void RestoreTextMode(int fd) {
  ioctl(fd, KDSETMODE, KD_TEXT);
}

void ArmConsoleGuard(int fd, RestoreFn restore) {
  g_restore.store(restore);
  g_fd.store(fd);
  if (!g_atexit) {
    g_atexit = atexit(&AtExit) == 0;
  }
  if (g_installed) {
    return;
  }
  struct sigaction sa;
  memset(&sa, 0, sizeof(sa));
  sa.sa_sigaction = &Handler;
  sa.sa_flags = SA_SIGINFO | SA_ONSTACK;
  sigemptyset(&sa.sa_mask);
  for (int i = 0; i < kCount; ++i) {
    sigaction(kSignals[i].sig, &sa, &g_old[i]);
  }
  g_installed = true;
}

void DisarmConsoleGuard() {
  g_fd.store(-1);
  if (!g_installed) {
    return;
  }
  for (int i = 0; i < kCount; ++i) {
    struct sigaction now;
    // Only put the old one back if ours is still in place.
    if (sigaction(kSignals[i].sig, nullptr, &now) == 0 &&
        (now.sa_flags & SA_SIGINFO) && now.sa_sigaction == &Handler) {
      sigaction(kSignals[i].sig, &g_old[i], nullptr);
    }
  }
  g_installed = false;
}

}  // namespace bitsos_fb
