// BitsOS V2 Cobalt backend: gives the text console back however Cobalt ends.
//
// With BITSOS_KD_GRAPHICS=1 the console is switched to graphics mode; if the
// program then dies without its normal teardown, the console would stay
// dark. Armed, this guard runs a restore function (KD_TEXT) at exit() and on
// fatal signals (SIGSEGV, SIGBUS, SIGFPE, SIGILL, SIGABRT), then hands the
// signal on to whatever handler was there before. Stop signals (SIGTERM,
// SIGINT, SIGHUP, SIGQUIT) restore only when nobody else handles them (a
// handler there means an orderly stop, whose teardown restores anyway).
// SIGKILL cannot be caught: georg-vt-check.sh restores after the process.
// Handlers installed later by Chromium may not pass signals on to this one;
// the script covers that too.

#ifndef STARBOARD_SHARED_BITSOS_FB_CONSOLE_GUARD_H_
#define STARBOARD_SHARED_BITSOS_FB_CONSOLE_GUARD_H_

namespace bitsos_fb {

// Must be async-signal-safe (a single ioctl, a write).
typedef void (*RestoreFn)(int fd);

// Arms the guard for |fd|; |restore| runs at most once.
void ArmConsoleGuard(int fd, RestoreFn restore);

// Normal end (the caller restores itself): puts the old handlers back.
void DisarmConsoleGuard();

// ioctl(fd, KDSETMODE, KD_TEXT).
void RestoreTextMode(int fd);

}  // namespace bitsos_fb

#endif  // STARBOARD_SHARED_BITSOS_FB_CONSOLE_GUARD_H_
