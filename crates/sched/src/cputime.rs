//! SC-6: CPU time per thread, split into user and system time, and the accumulators a process keeps.
//!
//! Every thread's CPU time is charged at each switch and accounting point (`run::account`). The split into
//! user and system time happens at **kernel entry and exit**: the process loop calls `user_enter()` right
//! before it enters user mode and `user_exit()` right after user mode returns to it (syscall, fault, a stop
//! after an interrupt), so the time in between is user time, everything else system time. This is Linux's
//! `VIRT_CPU_ACCOUNTING_GEN` precision (CachyOS runs with it: `vtime_user_enter/exit`), not tick sampling: the
//! numbers need no `cputime_adjust` scaling and never go back. Interrupts taken while a program runs count as
//! its user time (Linux without IRQ time accounting does the same).
//!
//! Readers on other CPUs see a thread's numbers **including its running slice** (AB-3 change request 6)
//! through a sequence counter: the accounting fields are written only by the CPU that runs the thread (or
//! picks it), with interrupts off; a reader retries while a write is in progress (bounded) and adds the time
//! since the slice began.
//!
//! `ProcessTimes` is what Linux keeps in `signal_struct`: the times of a process's threads that ended
//! (`utime`/`stime`) and of its children that were reaped by wait (`cutime`/`cstime`, `cnvcsw`, ...). The proc
//! side (AB) owns where it lives (`Process::cpu`, integration patch of SC-6) and calls `thread_ended` at a
//! thread's end and `reap` when wait4 collects a child.

use crate::{Thread, cpus, table};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

/// One thread's or process's CPU time. `nvcsw`/`nivcsw`: voluntary switches (it blocked) and involuntary ones
/// (preempted, or it yielded while still runnable), as Linux counts them in `__schedule`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimes {
    pub user_ns: u64,
    pub system_ns: u64,
    pub nvcsw: u64,
    pub nivcsw: u64,
}

impl CpuTimes {
    pub const ZERO: CpuTimes = CpuTimes { user_ns: 0, system_ns: 0, nvcsw: 0, nivcsw: 0 };
    /// user + system (Linux `sum_exec_runtime` for a thread of V2: both are measured, nothing is sampled).
    pub fn total_ns(&self) -> u64 {
        self.user_ns.saturating_add(self.system_ns)
    }
    pub fn plus(self, o: CpuTimes) -> CpuTimes {
        CpuTimes {
            user_ns: self.user_ns.saturating_add(o.user_ns),
            system_ns: self.system_ns.saturating_add(o.system_ns),
            nvcsw: self.nvcsw.saturating_add(o.nvcsw),
            nivcsw: self.nivcsw.saturating_add(o.nivcsw),
        }
    }
}

/// Set once the first time a thread entered user mode through `user_enter` (tests use it to see whether the
/// process loop marks user mode yet; no per-call counter: this sits on every syscall's path).
static USER_MARKED: AtomicBool = AtomicBool::new(false);

/// Whether any thread ever marked user mode (the proc side calls `user_enter`).
pub fn user_marked() -> bool {
    USER_MARKED.load(Ordering::Relaxed)
}

/// The calling thread enters user mode now: the time since its last mark was system time. Cheap (interrupts
/// off for a few atomics); no allocation, no lock.
pub fn user_enter() {
    mark(true);
    if !USER_MARKED.load(Ordering::Relaxed) {
        USER_MARKED.store(true, Ordering::Relaxed);
    }
}

/// The calling thread is back in the kernel: the time since `user_enter` was user time.
pub fn user_exit() {
    mark(false);
}

pub(crate) fn mark(user: bool) {
    if !crate::STARTED.load(Ordering::Acquire) {
        return;
    }
    let _irq = hal::cpu::IrqGuard::new();
    let Some(t) = crate::current_thread() else { return };
    if t.in_user.load(Ordering::Relaxed) == user {
        return;
    }
    crate::run::account_and_mark(cpus::this(), &t, time::now_ns(), Some(user));
}

/// Whether the calling thread is in user mode by its marks (between `user_enter` and `user_exit`).
pub fn in_user() -> bool {
    crate::current_thread().is_some_and(|t| t.in_user.load(Ordering::Relaxed))
}

/// Retries of a read while the owning CPU writes (a write is a handful of stores with interrupts off).
const READ_TRIES: u32 = 1000;

impl Thread {
    /// Begins a write of the accounting fields (only the CPU running or picking the thread, interrupts off).
    pub(crate) fn acct_begin(&self) {
        let s = self.acct_seq.load(Ordering::Relaxed);
        self.acct_seq.store(s.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
    }
    pub(crate) fn acct_end(&self) {
        let s = self.acct_seq.load(Ordering::Relaxed);
        self.acct_seq.store(s.wrapping_add(1), Ordering::Release);
    }
    /// Starts (`now`) or ends (0) the thread's running slice for readers.
    pub(crate) fn set_exec_start(&self, now: u64) {
        self.acct_begin();
        self.exec_start.store(now, Ordering::Relaxed);
        self.acct_end();
    }

    /// Voluntary switches so far (it blocked); RB-7a's slow-syscall trace asks it to leave out calls that slept.
    pub fn nvcsw(&self) -> u64 {
        self.nvcsw.load(Ordering::Relaxed)
    }

    /// Its CPU time up to now, the running slice included (consistent: user + system = all time it ran).
    pub fn cpu_times(&self) -> CpuTimes {
        let (nvcsw, nivcsw) = (self.nvcsw.load(Ordering::Relaxed), self.nivcsw.load(Ordering::Relaxed));
        for _ in 0..READ_TRIES {
            let s1 = self.acct_seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let run = self.runtime_ns.load(Ordering::Relaxed);
            let user = self.utime_ns.load(Ordering::Relaxed);
            let start = self.exec_start.load(Ordering::Relaxed);
            let in_user = self.in_user.load(Ordering::Relaxed);
            // The clock is read before the re-check: a slice that closed after this read changes the sequence and
            // the read is retried. Read after the check, a slice closing in between was counted up to a later
            // time than its owner closed it at, and the next read went backwards (INT-W3, FS-1b's gate).
            let now = if start == 0 { 0 } else { time::now_ns() };
            fence(Ordering::Acquire);
            if self.acct_seq.load(Ordering::Relaxed) != s1 {
                continue;
            }
            let open = now.saturating_sub(start);
            let user = user.saturating_add(if in_user { open } else { 0 });
            let total = run.saturating_add(open);
            return self.monotonic(user.min(total), total.saturating_sub(user), nvcsw, nivcsw);
        }
        // The owner kept writing (cannot happen for long): the closed slices only.
        let run = self.runtime_ns.load(Ordering::Relaxed);
        let user = self.utime_ns.load(Ordering::Relaxed).min(run);
        self.monotonic(user, run - user, nvcsw, nivcsw)
    }

    /// Never less than an earlier read: the open slice a reader adds runs to its own clock, while the owner ends
    /// the slice (`set_exec_start(0)` at the switch) some time after it last charged it, so that tail is seen by
    /// readers and then not charged.
    fn monotonic(&self, user: u64, sys: u64, nvcsw: u64, nivcsw: u64) -> CpuTimes {
        let user_ns = self.seen_user.fetch_max(user, Ordering::Relaxed).max(user);
        let system_ns = self.seen_sys.fetch_max(sys, Ordering::Relaxed).max(sys);
        CpuTimes { user_ns, system_ns, nvcsw, nivcsw }
    }
}

/// The calling thread's CPU time up to now.
pub fn current() -> CpuTimes {
    crate::current_thread().map_or(CpuTimes::ZERO, |t| t.cpu_times())
}

/// Thread `id`'s CPU time up to now (None: no such thread).
pub fn of_thread(id: u64) -> Option<CpuTimes> {
    table::get(id).map(|t| t.cpu_times())
}

/// Idle time of all CPUs together since boot (`/proc/uptime`'s second field, FS-2 change request 4).
pub fn idle_ns_total() -> u64 {
    crate::cpu_stats().iter().map(|c| c.idle_ns).fold(0u64, |a, b| a.saturating_add(b))
}

/// Four counters (atomics). Readers take them through `ProcessTimes`'s write counters, so a sum is never seen
/// half added.
#[derive(Default)]
struct Acc {
    user: AtomicU64,
    system: AtomicU64,
    nvcsw: AtomicU64,
    nivcsw: AtomicU64,
}

impl Acc {
    const fn new() -> Acc {
        Acc { user: AtomicU64::new(0), system: AtomicU64::new(0), nvcsw: AtomicU64::new(0), nivcsw: AtomicU64::new(0) }
    }
    fn add(&self, t: CpuTimes) {
        self.user.fetch_add(t.user_ns, Ordering::Relaxed);
        self.system.fetch_add(t.system_ns, Ordering::Relaxed);
        self.nvcsw.fetch_add(t.nvcsw, Ordering::Relaxed);
        self.nivcsw.fetch_add(t.nivcsw, Ordering::Relaxed);
    }
    fn get(&self) -> CpuTimes {
        CpuTimes {
            user_ns: self.user.load(Ordering::Relaxed),
            system_ns: self.system.load(Ordering::Relaxed),
            nvcsw: self.nvcsw.load(Ordering::Relaxed),
            nivcsw: self.nivcsw.load(Ordering::Relaxed),
        }
    }
}

/// A process's CPU-time accumulators (Linux `signal_struct`: utime/stime of ended threads, cutime/cstime of
/// children reaped by wait). The live threads' times are read from the threads themselves (`of_thread`).
///
/// Consistency (Linux reads these under `sig->stats_lock`, a seqlock): writers (`thread_ended`, `reap`; several
/// at once once AB-4 has threads) take the `writing` flag (interrupts off, a handful of atomic adds inside, so
/// a waiter spins only that long) and make `seq` odd while they add. A reader retries while `seq` is odd or
/// changed under it; if writers keep it busy for `READ_TRIES` rounds it takes the flag itself and reads with
/// no write in flight. So `snapshot` never returns an ended time without its thread count (the thread counted
/// twice, with its live part) or the count without the time (the thread dropped). Not a ksync lock: readers
/// and writers sit at any lock level (wait4 under proc's locks, readers in syscalls and MCP), and nothing is
/// taken inside. `seen_*`: what `monotonic` last reported, so a process's time never goes back between two
/// readers even across a thread's end (Linux's `cputime_adjust` keeps `prev_cputime` for the same reason).
#[derive(Default)]
pub struct ProcessTimes {
    writing: AtomicBool,
    seq: AtomicU64,
    ended: Acc,
    ended_threads: AtomicU64,
    children: Acc,
    seen_user: AtomicU64,
    seen_system: AtomicU64,
}

klog::counter!(pub PROCESS_TIMES_SLOW_READS, "sched.process_times_slow_reads");

impl ProcessTimes {
    pub const fn new() -> ProcessTimes {
        ProcessTimes {
            writing: AtomicBool::new(false),
            seq: AtomicU64::new(0),
            ended: Acc::new(),
            ended_threads: AtomicU64::new(0),
            children: Acc::new(),
            seen_user: AtomicU64::new(0),
            seen_system: AtomicU64::new(0),
        }
    }
    /// Runs `f` with the `writing` flag held and interrupts off (the holder is never interrupted, so a
    /// spinning CPU waits a few atomic operations at most).
    fn exclusive<R>(&self, f: impl FnOnce() -> R) -> R {
        let _irq = hal::cpu::IrqGuard::new();
        while self.writing.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            core::hint::spin_loop();
        }
        let r = f();
        self.writing.store(false, Ordering::Release);
        r
    }
    fn write(&self, f: impl FnOnce()) {
        self.exclusive(|| {
            let s = self.seq.load(Ordering::Relaxed);
            self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
            fence(Ordering::Release);
            f();
            self.seq.store(s.wrapping_add(2), Ordering::Release);
        });
    }
    /// `f` over the accumulators with no write half done.
    fn read<R>(&self, f: impl Fn() -> R) -> R {
        for _ in 0..READ_TRIES {
            let s1 = self.seq.load(Ordering::Acquire);
            if s1 & 1 == 1 {
                core::hint::spin_loop();
                continue;
            }
            let r = f();
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return r;
            }
        }
        // Writers kept it busy (many threads ending at once): read with them held off.
        if PROCESS_TIMES_SLOW_READS.get() == 0 {
            klog::event!(Debug, "sched", "process_times.slow_read", tries = READ_TRIES);
        }
        PROCESS_TIMES_SLOW_READS.inc();
        self.exclusive(f)
    }
    /// A thread of the process ended with `t` (called by the ending thread, `current()`, after its last user
    /// mode and before the process can be reaped). The thread must leave the process's list of live threads
    /// before or with this call, so a reader never counts it twice.
    pub fn thread_ended(&self, t: CpuTimes) {
        self.write(|| {
            self.ended.add(t);
            self.ended_threads.fetch_add(1, Ordering::Relaxed);
        });
    }
    /// The ended threads' times and their number, from one consistent moment.
    pub fn snapshot(&self) -> (CpuTimes, u64) {
        self.read(|| (self.ended.get(), self.ended_threads.load(Ordering::Relaxed)))
    }
    /// Threads that ended so far.
    pub fn ended_threads(&self) -> u64 {
        self.snapshot().1
    }
    /// The ended threads' times.
    pub fn ended(&self) -> CpuTimes {
        self.snapshot().0
    }
    /// Children reaped by wait (and, through them, their reaped children): Linux's cutime/cstime.
    pub fn children(&self) -> CpuTimes {
        self.read(|| self.children.get())
    }
    /// What wait4's rusage reports for this (ended) process: its own threads plus its reaped children
    /// (Linux `getrusage(child, RUSAGE_BOTH)` in `wait_task_zombie`).
    pub fn with_children(&self) -> CpuTimes {
        self.read(|| self.ended.get().plus(self.children.get()))
    }
    /// The parent's side of reaping `child` (Linux `wait_task_zombie`: `psig->cutime += tgutime + sig->cutime`).
    pub fn reap(&self, child: &ProcessTimes) {
        let t = child.with_children();
        self.write(|| self.children.add(t));
    }
    /// `t` (the process's time now: live threads + `snapshot`) raised to what was reported before, part by
    /// part, and remembered: user and system time never go back. Closes the gap of a thread that ran on a
    /// little after its `thread_ended` sample (a reader that saw it live counted that bit).
    pub fn monotonic(&self, t: CpuTimes) -> CpuTimes {
        let user = self.seen_user.fetch_max(t.user_ns, Ordering::AcqRel).max(t.user_ns);
        let system = self.seen_system.fetch_max(t.system_ns, Ordering::AcqRel).max(t.system_ns);
        CpuTimes { user_ns: user, system_ns: system, ..t }
    }
}

/// Linux clock ticks (`USER_HZ` = 100, AT_CLKTCK) of `ns`: `nsec_to_clock_t`.
pub fn clock_t(ns: u64) -> u64 {
    ns / (1_000_000_000 / USER_HZ)
}

/// Linux's `USER_HZ` (the unit of times(), /proc/<pid>/stat's utime/stime/start time; auxv AT_CLKTCK).
pub const USER_HZ: u64 = 100;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_times_add_up_like_signal_struct() {
        let p = ProcessTimes::new();
        assert_eq!(p.ended(), CpuTimes::ZERO);
        p.thread_ended(CpuTimes { user_ns: 5, system_ns: 7, nvcsw: 1, nivcsw: 2 });
        p.thread_ended(CpuTimes { user_ns: 1, system_ns: 1, nvcsw: 0, nivcsw: 1 });
        assert_eq!(p.ended_threads(), 2);
        assert_eq!(p.ended(), CpuTimes { user_ns: 6, system_ns: 8, nvcsw: 1, nivcsw: 3 });
        // A child with one ended thread and a reaped grandchild.
        let child = ProcessTimes::new();
        child.thread_ended(CpuTimes { user_ns: 100, system_ns: 10, nvcsw: 4, nivcsw: 0 });
        let grandchild = ProcessTimes::new();
        grandchild.thread_ended(CpuTimes { user_ns: 1000, system_ns: 0, nvcsw: 0, nivcsw: 9 });
        child.reap(&grandchild);
        assert_eq!(child.with_children(), CpuTimes { user_ns: 1100, system_ns: 10, nvcsw: 4, nivcsw: 9 });
        p.reap(&child);
        assert_eq!(p.children(), CpuTimes { user_ns: 1100, system_ns: 10, nvcsw: 4, nivcsw: 9 });
        assert_eq!(p.ended().user_ns, 6, "the children's times stay apart from its own");
        assert_eq!(p.with_children().total_ns(), 6 + 8 + 1110);
    }

    #[test]
    fn snapshot_holds_time_and_count_together() {
        let p = ProcessTimes::new();
        assert_eq!(p.snapshot(), (CpuTimes::ZERO, 0));
        p.thread_ended(CpuTimes { user_ns: 3, system_ns: 4, nvcsw: 0, nivcsw: 0 });
        assert_eq!(p.snapshot(), (CpuTimes { user_ns: 3, system_ns: 4, nvcsw: 0, nivcsw: 0 }, 1));
        assert_eq!(p.seq.load(Ordering::Relaxed), 2);
        assert!(!p.writing.load(Ordering::Relaxed));
    }

    #[test]
    fn snapshot_is_consistent_under_concurrent_writers() {
        extern crate std;
        use std::sync::Arc;
        let p = Arc::new(ProcessTimes::new());
        let writers: std::vec::Vec<_> = (0..4u32)
            .map(|_| {
                let p = p.clone();
                std::thread::spawn(move || {
                    for _ in 0..20_000 {
                        p.thread_ended(CpuTimes { user_ns: 10, system_ns: 1, nvcsw: 0, nivcsw: 0 });
                    }
                })
            })
            .collect();
        let mut last = 0;
        while writers.iter().any(|w| !w.is_finished()) {
            let (t, n) = p.snapshot();
            assert_eq!((t.user_ns, t.system_ns), (10 * n, n), "time and count from different moments");
            assert!(n >= last, "the count went back");
            last = n;
        }
        for w in writers {
            w.join().unwrap();
        }
        assert_eq!(p.snapshot().1, 80_000);
    }

    #[test]
    fn monotonic_never_goes_back() {
        let p = ProcessTimes::new();
        let a = p.monotonic(CpuTimes { user_ns: 100, system_ns: 50, nvcsw: 1, nivcsw: 2 });
        assert_eq!((a.user_ns, a.system_ns), (100, 50));
        // A later read that lost a few ns of an ending thread's last bit.
        let b = p.monotonic(CpuTimes { user_ns: 98, system_ns: 51, nvcsw: 1, nivcsw: 2 });
        assert_eq!((b.user_ns, b.system_ns, b.nvcsw), (100, 51, 1));
        let c = p.monotonic(CpuTimes { user_ns: 120, system_ns: 60, ..CpuTimes::ZERO });
        assert_eq!((c.user_ns, c.system_ns), (120, 60));
    }

    #[test]
    fn clock_ticks_as_linux() {
        assert_eq!(clock_t(0), 0);
        assert_eq!(clock_t(9_999_999), 0);
        assert_eq!(clock_t(10_000_000), 1);
        assert_eq!(clock_t(1_000_000_000), 100);
        assert_eq!(CpuTimes { user_ns: u64::MAX, system_ns: 1, nvcsw: 0, nivcsw: 0 }.total_ns(), u64::MAX);
    }
}
