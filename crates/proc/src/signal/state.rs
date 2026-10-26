//! The group's signal state behind `proc.signals`: queues, the threads' parts, sending, taking, job control.

use super::*;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::ab::siginfo::*;
use vfs::Errno;

mod timers;
pub use timers::*;

/// A pending set with its queued siginfos (`struct sigpending`).
#[derive(Clone, Debug, Default)]
pub struct SigQueue {
    pub set: u64,
    q: Vec<SigInfo>,
}

impl SigQueue {
    pub fn has(&self, sig: u32) -> bool {
        self.set & bit_checked(sig) != 0
    }
    pub fn queued(&self) -> usize {
        self.q.len()
    }
    /// `collect_signal`: the first queued entry of `sig` (zeros with SI_USER when none was queued: a
    /// fast-pathed or overflowed send); the bit stays set while another entry of `sig` is queued. The bool
    /// says whether an entry was taken.
    fn take(&mut self, sig: u32) -> (SigInfo, bool) {
        let first = self.q.iter().position(|i| i.signo() == sig);
        let more = first.is_some_and(|f| self.q[f + 1..].iter().any(|i| i.signo() == sig));
        if !more {
            self.set &= !bit(sig);
        }
        match first {
            Some(f) => (self.q.remove(f), true),
            None => (SigInfo::user(sig, SI_USER, 0, 0), false),
        }
    }
    /// `flush_sigqueue_mask`: drops every signal in `mask`; returns how many queued entries went.
    fn flush_mask(&mut self, mask: u64) -> usize {
        if self.set & mask == 0 {
            return 0;
        }
        self.set &= !mask;
        let before = self.q.len();
        self.q.retain(|i| bit_checked(i.signo()) & mask == 0);
        before - self.q.len()
    }
    fn clear(&mut self) -> usize {
        let n = self.q.len();
        self.set = 0;
        self.q.clear();
        n
    }
    /// `next_signal`: synchronous signals first, then the lowest number.
    fn next(&self, allowed: u64) -> Option<u32> {
        let x = self.set & allowed;
        if x == 0 {
            return None;
        }
        let x = if x & SYNCHRONOUS_MASK != 0 { x & SYNCHRONOUS_MASK } else { x };
        Some(x.trailing_zeros() + 1)
    }
    /// The first queued kernel-generated synchronous signal not in `blocked` (`dequeue_synchronous_signal`).
    fn take_synchronous(&mut self, blocked: u64) -> Option<SigInfo> {
        if self.set & !blocked & SYNCHRONOUS_MASK == 0 {
            return None;
        }
        let f = self.q.iter().position(|i| i.is_kernel_fault() && bit(i.signo()) & !blocked != 0)?;
        let sig = self.q[f].signo();
        if !self.q[f + 1..].iter().any(|i| i.signo() == sig) {
            self.set &= !bit(sig);
        }
        Some(self.q.remove(f))
    }
}

// ---------------------------------------------------------------------------------------------------------
// threads and the group

/// One thread's part (Linux `task_struct` fields under the siglock).
#[derive(Debug)]
pub struct ThreadSig {
    pub tid: u32,
    pub blocked: u64,
    /// sigtimedwait's mask while it waits with the waited-for set unblocked (`real_blocked`).
    pub real_blocked: u64,
    /// The mask sigsuspend/ppoll/pselect/epoll_pwait put back after the handler ran (`saved_sigmask` with
    /// TIF_RESTORE_SIGMASK).
    pub saved: Option<u64>,
    pub pending: SigQueue,
    /// JOBCTL_STOP_PENDING (it still counts in `group_stop_count` unless the group is stopped already).
    stop_pending: bool,
    /// JOBCTL_STOPPED: parked in a group stop.
    pub stopped: bool,
    /// TIF_SIGPENDING, shared with `Thread` (read without the lock on the way back to user mode).
    flag: Arc<AtomicBool>,
    /// The scheduler's id of the kernel thread running it (shared with `Thread`; 0 before it runs): kicks.
    kthread: Arc<AtomicU64>,
}

impl ThreadSig {
    fn new(tid: u32, blocked: u64, flag: Arc<AtomicBool>, kthread: Arc<AtomicU64>) -> ThreadSig {
        ThreadSig { tid, blocked: blocked & !UNBLOCKABLE, real_blocked: 0, saved: None, pending: SigQueue::default(), stop_pending: false, stopped: false, flag, kthread }
    }
    pub fn kthread(&self) -> u64 {
        self.kthread.load(Ordering::Acquire)
    }
    fn set_flag(&self, on: bool) {
        self.flag.store(on, Ordering::Release);
    }
    fn flag(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
    /// For host tests: TIF_SIGPENDING of this thread.
    pub fn flag_is_set(&self) -> bool {
        self.flag.load(Ordering::Acquire)
    }
}

/// Which threads a change must wake (their kernel threads' ids): the caller kicks them outside the lock.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Wake(pub Vec<u64>);

impl Wake {
    fn add(&mut self, kthread: u64) {
        if kthread != 0 && !self.0.contains(&kthread) && self.0.try_reserve(1).is_ok() {
            self.0.push(kthread);
        }
    }
    fn merge(&mut self, o: Wake) {
        for k in o.0 {
            self.add(k);
        }
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// What happened to a signal sent to a process or thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// Ignored (SIG_IGN, a default-ignore signal, init's unwanted signals, a dying group): dropped.
    Dropped,
    /// Recorded as pending (blocked, or with a handler, or a stop signal); a thread was told.
    Pending,
    /// Its default action ends the group: the caller ends it with this signal.
    Fatal,
}

/// Where a send goes (`PIDTYPE_PID` or `PIDTYPE_TGID`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum To {
    Group,
    Thread(u64),
}

/// The result of a send with the threads to wake.
#[derive(Debug)]
pub struct Outcome {
    pub sent: Sent,
    pub wake: Wake,
    /// A SIGCONT ended (or cut short) a stop: the parked threads wake.
    pub continued: bool,
}

/// A dequeued signal and what to do with it (`get_signal`'s answer).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// Run the handler with this action (the action as it was; SA_RESETHAND already applied to the table).
    Handler(SigInfo, SigAction),
    /// The default action ends the group.
    Fatal(SigInfo),
    /// The default action stops the group (the caller checks the orphaned-group rule first for
    /// SIGTSTP/SIGTTIN/SIGTTOU, then calls `begin_group_stop`).
    Stop(SigInfo),
}

/// Job-control notification the parent is owed (`SIGNAL_CLD_STOPPED` / `SIGNAL_CLD_CONTINUED`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cld {
    Stopped(u32),
    Continued,
}

/// The whole state behind `proc.signals`.
#[derive(Debug)]
pub struct Signals {
    actions: [SigAction; NSIG as usize],
    pub shared: SigQueue,
    threads: BTreeMap<u64, ThreadSig>,
    /// The thread `blocked` shows in `SignalState::with` (0: none yet).
    view: u64,
    /// The view thread's blocked mask (see the module doc).
    pub blocked: u64,
    /// SIGNAL_UNKILLABLE (init, pid 1): signals it has no handler for never reach it.
    pub unkillable: bool,
    /// The group is ending (SIGNAL_GROUP_EXIT): sends are dropped.
    pub exiting: bool,
    /// Threads still to stop in a group stop (`group_stop_count`).
    group_stop_count: u32,
    /// SIGNAL_STOP_STOPPED: every thread stopped.
    stop_stopped: bool,
    /// SIGNAL_STOP_CONTINUED: a continue wait4(WCONTINUED) has not reported yet.
    pub stop_continued: bool,
    /// The stop signal wait4(WUNTRACED) reports (`group_exit_code` of a stop; 0 once reported).
    pub stop_code: u32,
    /// Owed to the parent by the next thread that runs (`SIGNAL_CLD_MASK`).
    cld: Option<Cld>,
    /// `curr_target`: where the round robin for a group signal starts.
    curr_target: u64,
    /// Queued siginfos of this group (RLIMIT_SIGPENDING; Linux counts per user).
    queued: u64,
    /// Interval and POSIX timers (`timers.rs`; Linux's `signal_struct` keeps them under the siglock too).
    pub timers: Timers,
}

impl Default for Signals {
    fn default() -> Self {
        Signals {
            actions: [SigAction::default(); NSIG as usize],
            shared: SigQueue::default(),
            threads: BTreeMap::new(),
            view: 0,
            blocked: 0,
            unkillable: false,
            exiting: false,
            group_stop_count: 0,
            stop_stopped: false,
            stop_continued: false,
            stop_code: 0,
            cld: None,
            curr_target: 0,
            queued: 0,
            timers: Timers::default(),
        }
    }
}

impl Signals {
    pub fn action(&self, sig: u32) -> SigAction {
        if valid(sig) { self.actions[(sig - 1) as usize] } else { SigAction::default() }
    }

    /// `sig_handler_ignored`: SIG_IGN, or SIG_DFL for a signal whose default is to ignore (SIGCONT counts).
    pub fn ignored(&self, sig: u32) -> bool {
        match self.action(sig).handler {
            SIG_IGN => true,
            SIG_DFL => matches!(default_action(sig), DefaultAction::Ignore | DefaultAction::Continue),
            _ => false,
        }
    }

    /// `sig_fatal`: SIG_DFL and a default action that ends the process.
    pub fn fatal(&self, sig: u32) -> bool {
        self.action(sig).handler == SIG_DFL && default_kills(sig)
    }

    // ---- threads ----------------------------------------------------------------------------------------

    /// A new thread (spawn, fork, clone) with the mask it inherits. One joining a stopping group stops too
    /// (`task_join_group_stop`).
    pub fn add_thread(&mut self, key: u64, tid: u32, blocked: u64, flag: Arc<AtomicBool>, kthread: Arc<AtomicU64>) {
        let mut t = ThreadSig::new(tid, blocked, flag, kthread);
        if self.group_stop_count > 0 || self.stop_stopped {
            t.stop_pending = true;
            t.set_flag(true);
            if self.group_stop_count > 0 {
                self.group_stop_count += 1;
            }
        }
        if self.view == 0 {
            self.view = key;
            self.blocked = t.blocked;
        }
        self.threads.insert(key, t);
    }

    /// A thread ended (`exit_signals`): its own pending signals go; the shared ones it might have taken are
    /// passed to another thread (`retarget_shared_pending`); a stop it still owed counts as done (and may
    /// complete the group stop: the parent is owed CLD_STOPPED then).
    pub fn remove_thread(&mut self, key: u64) -> (Wake, Option<Cld>) {
        let mut wake = Wake::default();
        let mut cld = None;
        if let Some(mut t) = self.threads.remove(&key) {
            self.queued = self.queued.saturating_sub(t.pending.clear() as u64);
            if t.stop_pending && !self.exiting && self.participate_count() {
                cld = Some(Cld::Stopped(self.stop_code));
            }
            t.stop_pending = false;
            self.retarget(u64::MAX, &mut wake);
        }
        if self.view == key {
            self.view = self.main_key();
            self.blocked = self.blocked_of(self.view);
        }
        (wake, cld)
    }

    pub fn has_thread(&self, key: u64) -> bool {
        self.threads.contains_key(&key)
    }
    /// AB-7a: queued siginfos of the group (`/proc/<pid>/status` SigQ).
    pub fn queued_count(&self) -> u64 {
        self.queued
    }
    pub fn thread(&self, key: u64) -> Option<&ThreadSig> {
        self.threads.get(&key)
    }
    pub fn thread_keys(&self) -> Vec<u64> {
        self.threads.keys().copied().collect()
    }
    /// The main thread's key (the first one still there).
    pub fn main_key(&self) -> u64 {
        self.threads.keys().next().copied().unwrap_or(0)
    }

    /// Sets the view to `key` (the calling thread), or the main thread for 0 or an unknown key.
    pub fn set_view(&mut self, key: u64) {
        self.view = if self.threads.contains_key(&key) { key } else { self.main_key() };
        self.blocked = self.blocked_of(self.view);
    }
    /// The key of the thread run by kernel thread `kthread` (0: none of this group's).
    pub fn key_of_kthread(&self, kthread: u64) -> u64 {
        if kthread == 0 {
            return 0;
        }
        self.threads.iter().find(|(_, t)| t.kthread() == kthread).map_or(0, |(k, _)| *k)
    }
    pub fn view(&self) -> u64 {
        self.view
    }
    /// Writes a changed `blocked` view back to its thread.
    pub fn sync_view(&mut self) -> Wake {
        let v = self.view;
        let want = self.blocked & !UNBLOCKABLE;
        if self.threads.get(&v).is_some_and(|t| t.blocked != want) {
            return self.set_blocked(v, want);
        }
        self.blocked = want;
        Wake::default()
    }

    pub fn blocked_of(&self, key: u64) -> u64 {
        self.threads.get(&key).map_or(0, |t| t.blocked)
    }

    /// `__set_task_blocked`: a new mask for thread `key` (SIGKILL/SIGSTOP never blocked); shared signals it
    /// now blocks go to a thread that takes them; its flag is recalculated.
    pub fn set_blocked(&mut self, key: u64, mask: u64) -> Wake {
        let mut wake = Wake::default();
        let mask = mask & !UNBLOCKABLE;
        let Some(t) = self.threads.get_mut(&key) else { return wake };
        let newly = mask & !t.blocked;
        t.blocked = mask;
        if key == self.view {
            self.blocked = mask;
        }
        if newly != 0 {
            self.retarget(newly, &mut wake);
        }
        self.recalc(key);
        wake
    }

    /// `sigprocmask` on the view thread: returns the old mask. `how` is checked only with a new set (as
    /// `sys_rt_sigprocmask`). Shared signals the change hands to other threads are seen by them at their next
    /// return to user mode (the view callers kick nobody).
    pub fn sigprocmask(&mut self, how: u64, set: Option<u64>) -> Result<u64, Errno> {
        let v = self.view;
        self.sigprocmask_of(v, how, set).map(|(old, _)| old)
    }

    /// `sigprocmask` for thread `key`: the old mask and the threads to wake.
    pub fn sigprocmask_of(&mut self, key: u64, how: u64, set: Option<u64>) -> Result<(u64, Wake), Errno> {
        let old = self.blocked_of(key);
        let Some(s) = set else { return Ok((old, Wake::default())) };
        let s = s & !UNBLOCKABLE;
        let new = match how {
            SIG_BLOCK => old | s,
            SIG_UNBLOCK => old & !s,
            SIG_SETMASK => s,
            _ => return Err(EINVAL),
        };
        Ok((old, self.set_blocked(key, new)))
    }

    /// `rt_sigpending` for thread `key`: its and the group's pending signals that it blocks.
    pub fn pending_blocked_of(&self, key: u64) -> u64 {
        let t = self.threads.get(&key);
        (t.map_or(0, |t| t.pending.set) | self.shared.set) & t.map_or(0, |t| t.blocked)
    }
    /// `rt_sigpending` for the view thread.
    pub fn pending_blocked(&self) -> u64 {
        self.pending_blocked_of(self.view)
    }
    /// Thread `key`'s pending set and the group's (for /proc and the tests).
    pub fn pending_of(&self, key: u64) -> (u64, u64) {
        (self.threads.get(&key).map_or(0, |t| t.pending.set), self.shared.set)
    }

    /// `recalc_sigpending_tsk`: the thread has something to do on its way back to user mode.
    pub fn recalc(&mut self, key: u64) -> bool {
        let exiting = self.exiting;
        let shared = self.shared.set;
        let Some(t) = self.threads.get_mut(&key) else { return false };
        let on = exiting || t.stop_pending || (t.pending.set | shared) & !t.blocked != 0;
        t.set_flag(on);
        on
    }

    /// Whether thread `key` has an unblocked signal pending.
    pub fn deliverable(&self, key: u64) -> bool {
        self.threads.get(&key).is_some_and(|t| (t.pending.set | self.shared.set) & !t.blocked != 0)
    }

    /// `retarget_shared_pending(which)`: shared signals in `which` go to threads that do not block them.
    fn retarget(&mut self, which: u64, wake: &mut Wake) {
        let mut r = self.shared.set & which;
        if r == 0 {
            return;
        }
        for t in self.threads.values() {
            if r & !t.blocked == 0 {
                continue;
            }
            r &= t.blocked;
            if !t.flag() {
                t.set_flag(true);
                wake.add(t.kthread());
            }
            if r == 0 {
                break;
            }
        }
    }

    // ---- actions ------------------------------------------------------------------------------------------

    /// `do_sigaction`: returns the old action (flags masked as Linux shows them). Setting a signal to be
    /// ignored discards it from every queue of the group (POSIX 3.3.1.3).
    pub fn sigaction(&mut self, sig: u32, new: Option<SigAction>) -> Result<SigAction, Errno> {
        if !valid(sig) || (new.is_some() && bit(sig) & UNBLOCKABLE != 0) {
            return Err(EINVAL);
        }
        let mut old = self.action(sig);
        old.flags &= UAPI_SA_FLAGS;
        if let Some(mut a) = new {
            a.flags &= UAPI_SA_FLAGS;
            a.mask &= !UNBLOCKABLE;
            self.actions[(sig - 1) as usize] = a;
            if self.ignored(sig) {
                self.flush_everywhere(bit(sig));
            }
        }
        Ok(old)
    }

    fn flush_everywhere(&mut self, mask: u64) {
        let mut n = self.shared.flush_mask(mask);
        for t in self.threads.values_mut() {
            n += t.pending.flush_mask(mask);
        }
        self.queued = self.queued.saturating_sub(n as u64);
    }

    /// Linux `flush_signal_handlers(t, 0)` at execve: caught signals go back to SIG_DFL, ignored ones stay
    /// ignored; flags, restorer and masks are cleared. The blocked masks and pending signals stay.
    pub fn reset_for_exec(&mut self) {
        for a in self.actions.iter_mut() {
            let ign = a.handler == SIG_IGN;
            *a = SigAction { handler: if ign { SIG_IGN } else { SIG_DFL }, ..SigAction::default() };
        }
    }

    /// The state a fork or clone child group starts with (Linux `copy_process`): the actions (or SIG_DFL for
    /// the caught ones with CLONE_CLEAR_SIGHAND), no threads (the child's first thread joins with
    /// `add_thread` and the forking thread's mask), nothing pending, no job-control state.
    pub fn for_child(&self, clear_handlers: bool) -> Signals {
        let mut c = Signals { actions: self.actions, ..Signals::default() };
        if clear_handlers {
            for a in c.actions.iter_mut() {
                if a.handler != SIG_IGN {
                    a.handler = SIG_DFL;
                }
                a.flags = 0;
                a.restorer = 0;
                a.mask = 0;
            }
        }
        c
    }

    // ---- sending -----------------------------------------------------------------------------------------

    /// `sig_ignored` for thread `key` (a blocked signal is never ignored: it waits).
    fn sig_ignored(&self, key: u64, sig: u32, force: bool) -> bool {
        if let Some(t) = self.threads.get(&key)
            && (t.blocked | t.real_blocked) & bit(sig) != 0
        {
            return false;
        }
        let h = self.action(sig).handler;
        if self.unkillable && h == SIG_DFL && !(force && bit(sig) & UNBLOCKABLE != 0) {
            return true;
        }
        self.ignored(sig)
    }

    /// `prepare_signal`: a dying group drops everything; a stop signal removes pending SIGCONTs; SIGCONT
    /// removes pending stop signals, ends a stop and owes the parent CLD_CONTINUED (or CLD_STOPPED for a stop
    /// that was not complete yet). Returns false when the signal is ignored.
    fn prepare(&mut self, sig: u32, key: u64, force: bool, out: &mut Outcome) -> bool {
        if self.exiting {
            return false;
        }
        if bit(sig) & STOP_MASK != 0 {
            self.flush_everywhere(bit(SIGCONT));
        } else if sig == SIGCONT {
            self.flush_everywhere(STOP_MASK);
            let mut any = false;
            for t in self.threads.values_mut() {
                any |= t.stopped || t.stop_pending;
                t.stop_pending = false;
                t.stopped = false;
            }
            let why = if self.stop_stopped {
                Some(Cld::Continued)
            } else if self.group_stop_count > 0 {
                Some(Cld::Stopped(self.stop_code))
            } else {
                None
            };
            if let Some(w) = why {
                self.cld = Some(w);
                self.stop_continued = true;
                self.stop_stopped = false;
                self.group_stop_count = 0;
                self.stop_code = 0;
            }
            if any || why.is_some() {
                out.continued = true;
                for t in self.threads.values() {
                    // The thread that runs first owes the parent the notification: every thread looks.
                    t.set_flag(true);
                    out.wake.add(t.kthread());
                }
            }
        }
        !self.sig_ignored(key, sig, force)
    }

    /// `__send_signal_locked` + `complete_signal` (kernel/signal.c:1072, :1001). `limit` = RLIMIT_SIGPENDING.
    /// EAGAIN when a real-time signal sent with its own siginfo (not by kill) finds the queue full.
    pub fn send(&mut self, info: SigInfo, to: To, force: bool, limit: u64) -> Result<Outcome, Errno> {
        let sig = info.signo();
        let mut out = Outcome { sent: Sent::Dropped, wake: Wake::default(), continued: false };
        if !valid(sig) {
            return Err(EINVAL);
        }
        let shared = matches!(to, To::Group);
        let key = match to {
            To::Thread(k) if self.threads.contains_key(&k) => k,
            To::Thread(_) => return Err(ESRCH),
            To::Group => self.main_key(),
        };
        if !self.prepare(sig, key, force, &mut out) {
            return Ok(out);
        }
        let already = if shared { self.shared.has(sig) } else { self.threads.get(&key).is_some_and(|t| t.pending.has(sig)) };
        // `legacy_queue`: a standard signal already pending is not queued again.
        if sig < SIGRTMIN && already {
            out.sent = Sent::Pending;
            return Ok(out);
        }
        // SIGKILL never carries a siginfo; a standard signal from the kernel or kill() may exceed the limit.
        let mut store = sig != SIGKILL;
        if store && !(sig < SIGRTMIN && info.code() >= 0) && self.queued >= limit {
            if sig >= SIGRTMIN && info.code() != SI_USER {
                return Err(EAGAIN);
            }
            store = false;
        }
        let q = if shared {
            &mut self.shared
        } else {
            match self.threads.get_mut(&key) {
                Some(t) => &mut t.pending,
                None => return Err(ESRCH),
            }
        };
        if store {
            if q.q.try_reserve(1).is_ok() {
                q.q.push(info);
                self.queued += 1;
            } else if sig >= SIGRTMIN && info.code() != SI_USER {
                return Err(EAGAIN);
            }
        }
        q.set |= bit(sig);
        out.sent = self.complete(sig, key, shared, &mut out.wake);
        Ok(out)
    }

    /// `wants_signal` (without `task_curr`: a thread already told is not told again).
    fn wants(&self, sig: u32, key: u64) -> bool {
        let Some(t) = self.threads.get(&key) else { return false };
        if t.blocked & bit(sig) != 0 {
            return false;
        }
        if sig == SIGKILL {
            return true;
        }
        if t.stopped {
            return false;
        }
        !t.flag()
    }

    /// `complete_signal`: picks the thread to wake (the target first, then round robin), and starts the
    /// group's end at once for a fatal signal.
    fn complete(&mut self, sig: u32, target: u64, shared: bool, wake: &mut Wake) -> Sent {
        let chosen = if self.wants(sig, target) {
            Some(target)
        } else if !shared || self.threads.len() <= 1 {
            None
        } else {
            let keys: Vec<u64> = self.threads.keys().copied().collect();
            let start = keys.iter().position(|&k| k == self.curr_target).unwrap_or(0);
            let found = (0..keys.len()).map(|i| keys[(start + i) % keys.len()]).find(|&k| self.wants(sig, k));
            if let Some(k) = found {
                self.curr_target = k;
            }
            found
        };
        let Some(t) = chosen else { return Sent::Pending };
        // The group's end starts at once for a fatal signal, except a core-dump one: that is taken by the
        // thread (`get_signal`'s coredump path), which here also keeps a fault's own message (R3).
        if self.fatal(sig) && default_action(sig) != DefaultAction::Core && !self.exiting && self.threads.get(&t).is_some_and(|x| x.real_blocked & bit(sig) == 0) {
            return Sent::Fatal;
        }
        if let Some(x) = self.threads.get(&t) {
            x.set_flag(true);
            wake.add(x.kthread());
        }
        Sent::Pending
    }

    /// `force_sig_info_to_task` (kernel/signal.c:1316) for a fault or a bad frame: a blocked or ignored
    /// signal (or any, with `to_default`) gets SIG_DFL and is unblocked; SIGNAL_UNKILLABLE goes when the
    /// action is SIG_DFL; then it is sent to the thread.
    pub fn force(&mut self, key: u64, info: SigInfo, to_default: bool, limit: u64) -> Result<Outcome, Errno> {
        let sig = info.signo();
        if !valid(sig) {
            return Err(EINVAL);
        }
        let blocked = self.blocked_of(key) & bit(sig) != 0;
        let i = (sig - 1) as usize;
        if blocked || self.actions[i].handler == SIG_IGN || to_default {
            self.actions[i].handler = SIG_DFL;
            if blocked {
                let m = self.blocked_of(key) & !bit(sig);
                let _ = self.set_blocked(key, m);
            }
        }
        if self.actions[i].handler == SIG_DFL {
            self.unkillable = false;
        }
        self.send(info, To::Thread(key), true, limit)
    }

    // ---- taking signals ----------------------------------------------------------------------------------

    /// `dequeue_signal` for thread `key` with `allowed` (the signals it may take): its own queue first, then
    /// the group's. Returns the siginfo and whether it came from the group's queue.
    pub fn dequeue(&mut self, key: u64, allowed: u64) -> Option<(SigInfo, bool)> {
        self.dequeue_at(key, allowed, time::now_ns())
    }

    /// `dequeue` with the monotonic clock's `now` (the timers' re-arm; host tests pass it).
    pub fn dequeue_at(&mut self, key: u64, allowed: u64, now: u64) -> Option<(SigInfo, bool)> {
        let t = self.threads.get_mut(&key)?;
        let (info, queued, shared) = if let Some(sig) = t.pending.next(allowed) {
            let (i, q) = t.pending.take(sig);
            (i, q, false)
        } else if let Some(sig) = self.shared.next(allowed) {
            let (i, q) = self.shared.take(sig);
            (i, q, true)
        } else {
            return None;
        };
        if queued {
            self.queued = self.queued.saturating_sub(1);
        }
        let mut info = info;
        if (info.signo() == SIGALRM || info.code() == SI_TIMER) && self.timer_dequeued(&mut info, now) {
            crate::itimer::nudge();
        }
        self.recalc(key);
        Some((info, shared))
    }

    /// Whether thread `key` could take a signal of `set` now (signalfd's poll, sigtimedwait's look).
    pub fn has_pending_in(&self, key: u64, set: u64) -> bool {
        self.threads.get(&key).is_some_and(|t| (t.pending.set | self.shared.set) & set != 0)
    }

    /// `get_signal`'s loop for thread `key` (without the job-control and group-exit parts, which the caller
    /// does first): the next signal whose handling needs the thread. Ignored ones are dropped on the way.
    pub fn next_for(&mut self, key: u64) -> Option<Next> {
        loop {
            let blocked = self.blocked_of(key);
            let sync = self.threads.get_mut(&key)?.pending.take_synchronous(blocked);
            let info = match sync {
                Some(i) => {
                    self.queued = self.queued.saturating_sub(1);
                    self.recalc(key);
                    i
                }
                None => self.dequeue(key, !blocked)?.0,
            };
            let sig = info.signo();
            let a = self.action(sig);
            if a.handler == SIG_IGN {
                continue;
            }
            if a.handler != SIG_DFL {
                if a.flags & SA_RESETHAND != 0 {
                    self.actions[(sig - 1) as usize].handler = SIG_DFL;
                }
                return Some(Next::Handler(info, a));
            }
            match default_action(sig) {
                DefaultAction::Ignore | DefaultAction::Continue => continue,
                _ if self.unkillable && bit(sig) & UNBLOCKABLE == 0 => continue,
                DefaultAction::Stop => return Some(Next::Stop(info)),
                DefaultAction::Terminate | DefaultAction::Core => return Some(Next::Fatal(info)),
            }
        }
    }

    /// `signal_delivered`: the handler's mask (the thread's mask + sa_mask + the signal unless SA_NODEFER) and
    /// the saved mask is done with (it went into the frame).
    pub fn delivered(&mut self, key: u64, sig: u32, a: &SigAction) -> Wake {
        let mut m = self.blocked_of(key) | a.mask;
        if a.flags & SA_NODEFER == 0 {
            m |= bit(sig);
        }
        if let Some(t) = self.threads.get_mut(&key) {
            t.saved = None;
        }
        self.set_blocked(key, m)
    }

    /// The mask that goes into a frame (`sigmask_to_save`): the saved one when a temporary mask is in place.
    pub fn mask_to_save(&self, key: u64) -> u64 {
        self.threads.get(&key).map_or(0, |t| t.saved.unwrap_or(t.blocked))
    }
    /// `restore_saved_sigmask`: no handler ran, the temporary mask goes.
    pub fn restore_saved(&mut self, key: u64) -> Wake {
        match self.threads.get_mut(&key).and_then(|t| t.saved.take()) {
            Some(m) => self.set_blocked(key, m),
            None => Wake::default(),
        }
    }
    /// sigsuspend / ppoll / pselect: `mask` now, the old one after the handler (or at the call's end). Returns
    /// the old mask (None: no such thread).
    pub fn set_temporary(&mut self, key: u64, mask: u64) -> Option<(u64, Wake)> {
        let old = self.threads.get(&key)?.blocked;
        let w = self.set_blocked(key, mask);
        if let Some(t) = self.threads.get_mut(&key) {
            t.saved = Some(old);
        }
        Some((old, w))
    }
    /// Hands `old` over as the mask to put back after a handler (ppoll/pselect/epoll_pwait interrupted by a
    /// signal: `restore_saved_sigmask_unless(true)`).
    pub fn set_saved(&mut self, key: u64, old: u64) {
        if let Some(t) = self.threads.get_mut(&key) {
            t.saved = Some(old);
        }
        self.recalc(key);
    }
    pub fn saved_of(&self, key: u64) -> Option<u64> {
        self.threads.get(&key).and_then(|t| t.saved)
    }
    /// sigtimedwait: `real_blocked` = the mask, the waited-for set unblocked while it sleeps (Linux sets the
    /// mask directly here: unblocking is always fine).
    pub fn begin_timedwait(&mut self, key: u64, which: u64) {
        let view = self.view;
        let Some(t) = self.threads.get_mut(&key) else { return };
        t.real_blocked = t.blocked;
        t.blocked &= !which;
        if key == view {
            self.blocked = t.blocked;
        }
        self.recalc(key);
    }
    pub fn end_timedwait(&mut self, key: u64) -> Wake {
        let Some(t) = self.threads.get_mut(&key) else { return Wake::default() };
        let m = t.real_blocked;
        t.real_blocked = 0;
        self.set_blocked(key, m)
    }

    /// After the mask or an action changed outside a syscall exit (AB-3's `check_signals`, still used where a
    /// temporary mask is set before a wait): the first deliverable signal of thread `key` whose default action
    /// ends the process is taken and returned; ignored ones are dropped on the way. Handler and stop signals
    /// stay for the delivery on the way back to user mode.
    pub fn take_fatal_of(&mut self, key: u64) -> Option<u32> {
        let t = self.threads.get(&key)?;
        let mut ready = (t.pending.set | self.shared.set) & !t.blocked;
        while ready != 0 {
            let sig = ready.trailing_zeros() + 1;
            ready &= ready - 1;
            if self.ignored(sig) || (self.unkillable && self.action(sig).handler == SIG_DFL && bit(sig) & UNBLOCKABLE == 0) {
                self.flush_everywhere(bit(sig));
                continue;
            }
            if self.fatal(sig) {
                let _ = self.dequeue(key, bit(sig));
                return Some(sig);
            }
        }
        self.recalc(key);
        None
    }
    /// `take_fatal_of` for the view thread.
    pub fn take_fatal(&mut self) -> Option<u32> {
        let v = self.view;
        self.take_fatal_of(v)
    }

    // ---- job control -------------------------------------------------------------------------------------

    /// The group is ending: every thread is told (SIGNAL_GROUP_EXIT), any stop is over.
    pub fn begin_exit(&mut self) {
        self.exiting = true;
        self.group_stop_count = 0;
        for t in self.threads.values_mut() {
            t.stop_pending = false;
            t.stopped = false;
            t.set_flag(true);
        }
    }

    /// `do_signal_stop`'s first part for the thread that dequeued stop signal `sig`: every thread that is not
    /// stopped yet gets a stop to take part in. Returns the other threads to wake.
    pub fn begin_group_stop(&mut self, key: u64, sig: u32) -> Wake {
        let mut wake = Wake::default();
        if self.exiting || self.threads.get(&key).is_none_or(|t| t.stop_pending) {
            return wake;
        }
        if !self.stop_stopped {
            self.stop_code = sig;
        }
        self.group_stop_count = 0;
        for (k, t) in self.threads.iter_mut() {
            if !t.stopped && !t.stop_pending {
                t.stop_pending = true;
                self.group_stop_count += 1;
                t.set_flag(true);
                if *k != key {
                    wake.add(t.kthread());
                }
            }
        }
        wake
    }

    fn participate_count(&mut self) -> bool {
        self.group_stop_count = self.group_stop_count.saturating_sub(1);
        if self.group_stop_count == 0 && !self.stop_stopped {
            self.stop_stopped = true;
            self.stop_continued = false;
            return true;
        }
        false
    }

    /// `task_participate_group_stop` + JOBCTL_STOPPED for thread `key` if it has a stop to take part in.
    /// Some(notify) when it stopped: notify = it completed a fresh group stop (the parent gets CLD_STOPPED).
    pub fn participate(&mut self, key: u64) -> Option<bool> {
        let t = self.threads.get_mut(&key)?;
        if !t.stop_pending {
            return None;
        }
        t.stop_pending = false;
        t.stopped = true;
        let notify = self.participate_count();
        self.recalc(key);
        Some(notify)
    }
    pub fn is_stopped(&self, key: u64) -> bool {
        self.threads.get(&key).is_some_and(|t| t.stopped)
    }
    pub fn stop_pending(&self, key: u64) -> bool {
        self.threads.get(&key).is_some_and(|t| t.stop_pending)
    }
    /// The whole group is stopped (SIGNAL_STOP_STOPPED).
    pub fn group_stopped(&self) -> bool {
        self.stop_stopped
    }
    /// The notification owed to the parent, taken by the first thread that runs.
    pub fn take_cld(&mut self) -> Option<Cld> {
        self.cld.take()
    }

    /// The queued count (tests, /proc's SigQ).
    pub fn queued(&self) -> u64 {
        self.queued
    }
    /// SigIgn / SigCgt for /proc (`collect_sigign_sigcatch`).
    pub fn ign_cgt(&self) -> (u64, u64) {
        let (mut ign, mut cgt) = (0, 0);
        for s in 1..=NSIG {
            match self.action(s).handler {
                SIG_IGN => ign |= bit(s),
                SIG_DFL => {}
                _ => cgt |= bit(s),
            }
        }
        (ign, cgt)
    }
}

impl Wake {
    /// Joins two wake lists (`Signals` operations that do several things).
    pub fn and(mut self, o: Wake) -> Wake {
        self.merge(o);
        self
    }
}

/// `Signals` behind the group's lock (`proc.signals`; nothing but the scheduler's wake-ups, klog and memory is
/// taken under it).
pub struct SignalState {
    inner: SpinLock<Signals>,
}

impl Default for SignalState {
    fn default() -> Self {
        SignalState { inner: SpinLock::named("proc.signals", Signals::default()) }
    }
}

impl SignalState {
    pub fn new(s: Signals) -> Self {
        SignalState { inner: SpinLock::named("proc.signals", s) }
    }
    /// Runs `f` with the view set to the calling thread (when it belongs to this group, else the main
    /// thread); a changed `blocked` is written back to that thread.
    pub fn with<R>(&self, f: impl FnOnce(&mut Signals) -> R) -> R {
        let k = sched::current_id();
        let mut s = self.inner.lock();
        let key = s.key_of_kthread(k);
        s.set_view(key);
        let r = f(&mut s);
        let _ = s.sync_view();
        r
    }
    /// Runs `f` on the state as it is (thread-aware callers pass keys themselves).
    pub fn raw<R>(&self, f: impl FnOnce(&mut Signals) -> R) -> R {
        f(&mut self.inner.lock())
    }
    /// The calling thread's blocked mask (the main thread's for kernel-thread callers).
    pub fn blocked(&self) -> u64 {
        self.with(|s| s.blocked)
    }
    /// Anything pending that the calling thread does not block.
    pub fn has_unblocked_pending(&self) -> bool {
        self.with(|s| {
            let v = s.view();
            s.deliverable(v)
        })
    }
}
