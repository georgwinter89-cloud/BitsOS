//! AB-6: the signal calls' work on a thread (the syscall layer checks arguments and copies; this file does
//! what Linux does under the siglock): masks, sigsuspend, pause, sigtimedwait, signalfd's dequeue, and the
//! temporary masks of ppoll/pselect/epoll_pwait for FS (linux-6.6/kernel/signal.c :3177, :4553, :3601;
//! fs/signalfd.c :153; `set_user_sigmask`/`restore_saved_sigmask_unless`).

use crate::group::{Group, kick};
use crate::signal::{SigInfo, UNBLOCKABLE};
use crate::task::Thread;
use uapi::ab::siginfo::{ERESTARTNOHAND, ERESTARTSYS};
use uapi::errno::*;
use vfs::Errno;

/// How long a signal wait sleeps before it looks again (a kick wakes it at once; B4: no endless sleep).
const SLICE_NS: u64 = 1_000_000_000;

impl Thread {
    /// `sigprocmask(how, set)`: the old mask.
    pub fn sigprocmask(&self, how: u64, set: Option<u64>) -> Result<u64, Errno> {
        let key = self.sig_key();
        let (old, w) = self.group().signals.raw(|s| s.sigprocmask_of(key, how, set))?;
        kick(&w);
        Ok(old)
    }

    /// `rt_sigpending`: the pending signals (its own and the group's) it blocks.
    pub fn pending_blocked(&self) -> u64 {
        let key = self.sig_key();
        self.group().signals.raw(|s| s.pending_blocked_of(key))
    }

    /// Sleeps until `cond` or a signal to look at (or the group's end), in slices.
    fn sleep_until(&self, deadline: Option<u64>, cond: &dyn Fn() -> bool) -> bool {
        let g = self.group();
        loop {
            if cond() || self.signal_pending() {
                return true;
            }
            let slice = match deadline {
                Some(d) => {
                    let now = time::now_ns();
                    if now >= d {
                        return false;
                    }
                    (d - now).min(SLICE_NS)
                }
                None => SLICE_NS,
            };
            g.sig_poll.wait_until_timeout(|| cond() || self.signal_pending(), slice);
        }
    }

    /// `rt_sigsuspend(mask)`: waits with `mask` until a signal is to be handled; the old mask is put back after
    /// the handler (or at once when none runs). Answers ERESTARTNOHAND (the way out makes it EINTR).
    pub fn sigsuspend(&self, mask: u64) -> i64 {
        let key = self.sig_key();
        if let Some((_, w)) = self.group().signals.raw(|s| s.set_temporary(key, mask)) {
            kick(&w);
        }
        self.sleep_until(None, &|| false);
        -ERESTARTNOHAND
    }

    /// `pause()`.
    pub fn pause(&self) -> i64 {
        self.sleep_until(None, &|| false);
        -ERESTARTNOHAND
    }

    /// `do_sigtimedwait`: takes a pending signal of `which` (blocked or not); without one waits up to
    /// `timeout_ns` (None: forever) with `which` unblocked. EAGAIN when the time ran out, EINTR when another
    /// signal woke it (the way out delivers that one).
    pub fn sigtimedwait(&self, which: u64, timeout_ns: Option<u64>) -> Result<SigInfo, Errno> {
        let g = self.group();
        let key = self.sig_key();
        let want = which & !UNBLOCKABLE;
        if let Some((i, _)) = g.signals.raw(|s| s.dequeue(key, want)) {
            return Ok(i);
        }
        if timeout_ns == Some(0) {
            return Err(EAGAIN);
        }
        g.signals.raw(|s| s.begin_timedwait(key, want));
        let deadline = timeout_ns.map(|t| time::now_ns().saturating_add(t));
        let woke = self.sleep_until(deadline, &|| false);
        let (got, w) = g.signals.raw(|s| {
            let w = s.end_timedwait(key);
            (s.dequeue(key, want), w)
        });
        kick(&w);
        match got {
            Some((i, _)) => Ok(i),
            None if woke => Err(EINTR),
            None => Err(EAGAIN),
        }
    }

    /// signalfd's `signalfd_dequeue`: a pending signal of `mask` for this thread (its own queue, then the
    /// group's); without one EAGAIN (`nonblock`) or a wait, which a signal to handle ends with ERESTARTSYS.
    pub fn signalfd_dequeue(&self, mask: u64, nonblock: bool) -> Result<SigInfo, Errno> {
        let g = self.group();
        let key = self.sig_key();
        let want = mask & !UNBLOCKABLE;
        loop {
            if let Some((i, _)) = g.signals.raw(|s| s.dequeue(key, want)) {
                return Ok(i);
            }
            if nonblock {
                return Err(EAGAIN);
            }
            if g.group_exited() {
                return Err(EINTR);
            }
            if self.signal_pending() {
                return Err(ERESTARTSYS);
            }
            let ready = || g.signals.raw(|s| s.has_pending_in(key, want));
            self.sleep_until(None, &ready);
        }
    }

    /// signalfd's poll: a signal of `mask` is pending for this thread.
    pub fn has_pending_in(&self, mask: u64) -> bool {
        let key = self.sig_key();
        self.group().signals.raw(|s| s.has_pending_in(key, mask & !UNBLOCKABLE))
    }
}

impl Group {
    /// The thread of this group the calling kernel thread runs, else its main thread (kernel callers).
    fn caller_key(&self) -> u64 {
        let k = sched::current_id();
        self.signals.raw(|s| match s.key_of_kthread(k) {
            0 => s.main_key(),
            key => key,
        })
    }

    /// `set_user_sigmask` for FS's ppoll/pselect6/epoll_pwait(2): `mask` on the calling thread (SIGKILL and
    /// SIGSTOP stay deliverable); returns (old, now in place). A default-fatal signal it unblocks ends the
    /// process at once (so the wait ends with EINTR).
    pub fn sigmask_for_wait(&self, mask: u64) -> Option<(u64, u64)> {
        let key = self.caller_key();
        let (old, now, w) = self.signals.raw(|s| {
            let old = s.blocked_of(key);
            let w = s.set_blocked(key, mask);
            (old, s.blocked_of(key), w)
        });
        kick(&w);
        self.check_signals();
        Some((old, now))
    }

    /// `restore_saved_sigmask_unless(interrupted)` for FS: while `temp` is still the calling thread's mask,
    /// `old` comes back now, or, when the call ends with EINTR for a signal, after its handler ran (the frame
    /// carries `old`, Linux's saved_sigmask).
    pub fn sigmask_after_wait(&self, old: u64, temp: u64, interrupted: bool) {
        let key = self.caller_key();
        let w = self.signals.raw(|s| {
            if s.blocked_of(key) != temp {
                return crate::signal::Wake::default();
            }
            if interrupted {
                s.set_saved(key, old);
                crate::signal::Wake::default()
            } else {
                s.set_blocked(key, old)
            }
        });
        kick(&w);
    }
}
