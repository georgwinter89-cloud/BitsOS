//! A thread's seccomp state (linux-6.6/include/linux/seccomp_types.h `struct seccomp`): the mode and the
//! newest filter of its stack, and what changes them: strict mode, attaching a filter, TSYNC from a sibling,
//! fork/clone (`copy_seccomp`), a kill action (MODE_DEAD). execve keeps all of it.
//!
//! The dispatcher's cost when no mode is set is one load of `mode` and one branch. With a filter, a call the
//! stack allows by its number alone (the action cache, Linux's `seccomp_cache_check_allow`) is answered from
//! the thread's own copy of the cache words (atomic loads, no lock). Any other call takes the thread's own
//! lock for a moment to clone its `Head` (an `Arc` only this thread hands out, so threads that share one stack
//! do not share a reference count on every call) and runs the stack unlocked. A sibling's TSYNC writes the
//! narrower cache words before the new stack, so a racing call sees the old answer or a stricter one, never a
//! wider one than the stack it then runs.
//!
//! Changes take `seccomp.change` (a sleeping lock over all of seccomp's attach/TSYNC work, Linux's
//! siglock + cred_guard_mutex for this purpose; attaching is rare). Lock order: `seccomp.change` >
//! `proc.threads` (the TSYNC walk) > `seccomp.head` (per thread, a leaf above the heap).

use crate::consts::*;
use crate::filter::{CACHE_NRS, CACHE_WORDS, Data, Filter, action_rank};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use ksync::{Mutex, MutexGuard, SpinLock};

/// The per-thread holder of the stack's top (see the module text).
#[derive(Debug)]
struct Head {
    top: Arc<Filter>,
}

/// A thread's seccomp state.
pub struct TaskSeccomp {
    mode: AtomicU8,
    /// The top filter's action cache (all zero without a stack).
    allow: [AtomicU64; CACHE_WORDS],
    head: SpinLock<Option<Arc<Head>>>,
}

impl Default for TaskSeccomp {
    fn default() -> Self {
        Self::new()
    }
}

/// What the check decided for one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Run the call (ALLOW, or LOG with `log`).
    Allow { log: bool },
    /// Skip it and return -errno (ERRNO capped at MAX_ERRNO; TRACE without a tracer and USER_NOTIF without a
    /// listener are ENOSYS).
    Errno { errno: u32, action: u32, log: bool },
    /// Skip it and raise SIGSYS (si_errno = data).
    Trap { data: u16, log: bool },
    /// End this thread, or the process when it is the last thread, with SIGSYS.
    KillThread { data: u16 },
    /// End the process with SIGSYS (KILL_PROCESS and every unknown action).
    KillProcess { data: u16 },
    /// Strict mode refused the call, or the thread is past a kill: SIGKILL for this thread.
    StrictKill,
}

/// Proof that `seccomp.change` is held.
pub struct ChangeGuard(#[allow(dead_code)] MutexGuard<'static, ()>);

static CHANGE: Mutex<()> = Mutex::named("seccomp.change", ());

/// Takes `seccomp.change`.
pub fn change() -> ChangeGuard {
    ChangeGuard(CHANGE.lock())
}

/// Why attaching failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachError {
    /// Another mode is set (EINVAL).
    Mode,
    /// The stack would pass MAX_INSNS_PER_PATH (ENOMEM).
    TooLong,
}

impl TaskSeccomp {
    pub const fn new() -> TaskSeccomp {
        TaskSeccomp { mode: AtomicU8::new(MODE_DISABLED), allow: [const { AtomicU64::new(0) }; CACHE_WORDS], head: SpinLock::named("seccomp.head", None) }
    }

    /// The mode (0 disabled, 1 strict, 2 filter, 3 dead): the dispatcher's one branch.
    #[inline(always)]
    pub fn mode(&self) -> u8 {
        self.mode.load(Ordering::Acquire)
    }
    /// /proc's `Seccomp_filters`.
    pub fn filter_count(&self) -> u32 {
        self.head.lock().as_ref().map_or(0, |h| h.top.depth())
    }
    fn top(&self) -> Option<Arc<Filter>> {
        self.head.lock().as_ref().map(|h| h.top.clone())
    }
    fn set(&self, top: Option<Arc<Filter>>, mode: u8) {
        let words = top.as_ref().map_or([0u64; CACHE_WORDS], |t| *t.allow_words());
        let head = top.map(|top| Arc::new(Head { top }));
        let old = {
            let mut g = self.head.lock();
            // A new stack only narrows the old one's cache (or replaces a missing one): the words go first.
            for (a, w) in self.allow.iter().zip(words) {
                a.store(w, Ordering::Release);
            }
            let old = core::mem::replace(&mut *g, head);
            self.mode.store(mode, Ordering::Release);
            old
        };
        drop(old);
    }

    fn snapshot(&self) -> (Option<Arc<Filter>>, u8) {
        let g = self.head.lock();
        (g.as_ref().map(|h| h.top.clone()), self.mode())
    }

    /// fork/clone (`copy_seccomp`): the new thread starts with its creator's mode and stack.
    pub fn inherit(&self, from: &TaskSeccomp) {
        let (top, mode) = from.snapshot();
        self.set(top, mode);
    }

    /// A new thread entering its group's thread list, under the list lock TSYNC's walk takes: it takes its
    /// creator's mode and stack again when a TSYNC synced the creator after `inherit` (Linux copies them under
    /// the siglock its TSYNC holds, linux-6.6/kernel/fork.c `copy_seccomp`). True when anything changed.
    pub fn resync(&self, from: &TaskSeccomp) -> bool {
        let (top, mode) = from.snapshot();
        let same = mode == self.mode()
            && match (&top, self.top()) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, &b),
                _ => false,
            };
        if !same {
            self.set(top, mode);
        }
        !same
    }

    /// The check at every syscall entry (`__secure_computing`).
    pub fn check(&self, d: &Data) -> Verdict {
        match self.mode() {
            MODE_DISABLED => Verdict::Allow { log: false },
            MODE_STRICT => {
                if STRICT_ALLOWED.contains(&d.nr) {
                    Verdict::Allow { log: false }
                } else {
                    Verdict::StrictKill
                }
            }
            MODE_FILTER => {
                let nr = d.nr as usize;
                if d.arch == AUDIT_ARCH_X86_64 && nr < CACHE_NRS && self.allow[nr / 64].load(Ordering::Acquire) & (1u64 << (nr % 64)) != 0 {
                    return Verdict::Allow { log: false };
                }
                let head = self.head.lock().clone();
                match head {
                    // Fail closed, as `seccomp_run_filters` does for a missing filter.
                    None => Verdict::KillProcess { data: 0 },
                    Some(h) => verdict(h.top.run(d)),
                }
            }
            _ => Verdict::StrictKill,
        }
    }

    /// After a kill action: whatever happens next, this thread runs no further call.
    pub fn set_dead(&self) {
        self.mode.store(MODE_DEAD, Ordering::Release);
    }

    /// `seccomp_may_assign_mode`.
    pub fn may_assign(&self, mode: u8) -> bool {
        let m = self.mode();
        m == MODE_DISABLED || m == mode
    }

    /// SECCOMP_SET_MODE_STRICT.
    pub fn set_strict(&self, _g: &ChangeGuard) -> Result<(), AttachError> {
        if !self.may_assign(MODE_STRICT) {
            return Err(AttachError::Mode);
        }
        self.mode.store(MODE_STRICT, Ordering::Release);
        Ok(())
    }

    /// The first checks of `seccomp_attach_filter` (mode, then length), before TSYNC's.
    pub fn can_attach(&self, _g: &ChangeGuard, prog: &crate::bpf::Program) -> Result<(), AttachError> {
        if !self.may_assign(MODE_FILTER) {
            return Err(AttachError::Mode);
        }
        let top = self.top();
        if !Filter::fits(prog, top.as_deref()) {
            return Err(AttachError::TooLong);
        }
        Ok(())
    }

    /// Puts `prog` on top of this thread's stack (the caller ran `can_attach` under the same guard).
    pub fn attach(&self, g: &ChangeGuard, prog: crate::bpf::Program, log: bool) -> Result<u32, AttachError> {
        self.can_attach(g, &prog)?;
        let f = Arc::new(Filter::new(prog, log, self.top()));
        let depth = f.depth();
        self.set(Some(f), MODE_FILTER);
        Ok(depth)
    }

    /// `seccomp_can_sync_threads` for one sibling: it has no mode, or a filter stack the caller's contains.
    pub fn can_sync_from(&self, caller: &TaskSeccomp) -> bool {
        match self.mode() {
            MODE_DISABLED => true,
            MODE_FILTER => match (self.top(), caller.top()) {
                (None, _) => true,
                (Some(mine), Some(theirs)) => theirs.has(&mine),
                (Some(_), None) => false,
            },
            _ => false,
        }
    }

    /// Whether this thread's stack already contains the caller's top.
    pub fn synced_with(&self, caller: &TaskSeccomp) -> bool {
        match (self.top(), caller.top()) {
            (Some(mine), Some(theirs)) => self.mode() == MODE_FILTER && mine.has(&theirs),
            _ => false,
        }
    }

    /// `seccomp_sync_threads` for one sibling: it takes the caller's stack and filter mode.
    pub fn sync_from(&self, _g: &ChangeGuard, caller: &TaskSeccomp) {
        self.set(caller.top(), MODE_FILTER);
    }
}

/// `__seccomp_filter`'s switch over the stack's answer.
fn verdict((ret, log): (u32, bool)) -> Verdict {
    let data = (ret & RET_DATA) as u16;
    match ret & RET_ACTION_FULL {
        RET_ALLOW => Verdict::Allow { log: false },
        RET_LOG => Verdict::Allow { log: true },
        RET_ERRNO => Verdict::Errno { errno: (data as u32).min(MAX_ERRNO), action: RET_ERRNO, log },
        RET_TRAP => Verdict::Trap { data, log },
        RET_TRACE => Verdict::Errno { errno: uapi::errno::ENOSYS as u32, action: RET_TRACE, log },
        RET_USER_NOTIF => Verdict::Errno { errno: uapi::errno::ENOSYS as u32, action: RET_USER_NOTIF, log },
        RET_KILL_THREAD => Verdict::KillThread { data },
        _ => Verdict::KillProcess { data },
    }
}

/// SECCOMP_GET_ACTION_AVAIL: the actions this kernel knows.
pub fn action_available(action: u32) -> bool {
    matches!(action, RET_KILL_PROCESS | RET_KILL_THREAD | RET_TRAP | RET_ERRNO | RET_USER_NOTIF | RET_TRACE | RET_LOG | RET_ALLOW)
}

/// For tests and the log: the rank order of two return values (lower wins).
pub fn wins(a: u32, b: u32) -> bool {
    action_rank(a) < action_rank(b)
}
