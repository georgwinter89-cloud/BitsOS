//! SC-6 syscall glue (granted): CPU time for programs.
//! - `times` (linux-6.6/kernel/sys.c:1023): the process's user/system time and its reaped children's, in
//!   clock ticks (`USER_HZ` 100); the answer is the ticks since boot (Linux adds `INITIAL_JIFFIES`, an
//!   arbitrary start; programs only take differences);
//! - `getrusage` (kernel/sys.c:1858, `getrusage` :1781): RUSAGE_SELF / CHILDREN / THREAD, times as timevals,
//!   voluntary/involuntary switches, ru_maxrss the peak resident set (AB-7c: MM's high-water mark and the
//!   group's earlier images; for CHILDREN the largest peak of the collected children, Linux `cmaxrss`);
//!   page-fault and block-I/O counts are 0 until MM/FS count them;
//! - the CPU clocks of `clock_gettime`/`clock_getres`/`clock_nanosleep` (kernel/time/posix-cpu-timers.c:
//!   `pid_for_clock` :57, `posix_cpu_clock_getres` :160, `posix_cpu_clock_get` :358, `posix_cpu_nsleep`
//!   :1592): PROF = user + system, VIRT = user, SCHED = all run time (the same as PROF in V2, where nothing is
//!   sampled); a thread clock only of the caller's own thread group; resolution 1 ns for SCHED clocks and
//!   1 ms (Linux at CachyOS's HZ=1000) for PROF/VIRT;
//! - wait4's rusage of the collected child.
//!
//! A process's time = its live threads' (`sched::cputime::of_thread`, the running slice included) + its ended
//! threads' (`ProcessTimes` in `proc::Group::cpu`; AB-5 applied SC-6's integration patch on AB-4's threads).

use crate::SysCtx;
use proc::Process;
use sched::cputime::{self, CpuTimes};
use uapi::errno::*;

const NSEC: u64 = 1_000_000_000;
/// Linux's `CPUCLOCK_*` kinds (the low two bits of a CPU clock id).
pub(crate) const CPUCLOCK_PROF: u8 = 0;
pub(crate) const CPUCLOCK_VIRT: u8 = 1;
pub(crate) const CPUCLOCK_SCHED: u8 = 2;
const RUSAGE_SELF: i32 = 0;
const RUSAGE_CHILDREN: i32 = -1;
const RUSAGE_THREAD: i32 = 1;
const TIMER_ABSTIME: u64 = 1;

/// All CPU time of process `p` (Linux `thread_group_cputime`): its live threads (the running slices included)
/// plus its ended threads, never less than reported before (`proc::Group::cpu_times`, AB-5 with SC-6's
/// accumulators in the group).
pub(crate) fn process_times(p: &Process) -> CpuTimes {
    p.cpu_times()
}

/// Its reaped children's time (Linux cutime/cstime).
fn children_times(p: &Process) -> CpuTimes {
    p.cpu.children()
}

/// The times wait4/waitid report for a collected child (its own and its reaped children's).
pub(crate) fn reaped_times(r: &proc::Reaped) -> CpuTimes {
    r.times
}

/// One thread's time (0 once it ended: its time went to its process).
fn thread_times(t: &proc::Thread) -> CpuTimes {
    cputime::of_thread(t.kthread()).unwrap_or(CpuTimes::ZERO)
}

/// The `which` sample of `t` (PROF = user + system, VIRT = user, SCHED = all run time).
fn sample(t: CpuTimes, which: u8) -> u64 {
    match which {
        CPUCLOCK_VIRT => t.user_ns,
        CPUCLOCK_PROF | CPUCLOCK_SCHED => t.total_ns(),
        // `clock::decode` refuses the fourth kind.
        _ => t.total_ns(),
    }
}

/// What a CPU clock names (`pid_for_clock`, linux-6.6/kernel/time/posix-cpu-timers.c:57).
enum Target {
    Thread(alloc::sync::Arc<proc::Thread>),
    Process(alloc::sync::Arc<Process>),
}

impl Target {
    fn times(&self) -> CpuTimes {
        match self {
            Target::Thread(t) => thread_times(t),
            Target::Process(p) => process_times(p),
        }
    }
    /// The thread ended or the process was collected (a sleep on its clock ends).
    fn gone(&self) -> bool {
        match self {
            Target::Thread(t) => proc::find_thread(t.tid()).is_none(),
            Target::Process(p) => proc::find(p.pid).is_none(),
        }
    }
}

/// `pid_for_clock(clock, gettime)`: pid 0 = the calling thread or its process; a thread clock names a thread of
/// the caller's own group; a process clock names a process by its pid (a zombie too, until reaped), or, for
/// clock_gettime only, the caller's own thread id. None = EINVAL.
fn clock_target(c: &SysCtx<'_>, pid: u32, thread: bool, gettime: bool) -> Option<Target> {
    if pid == 0 {
        return Some(if thread { Target::Thread(c.thread.clone()) } else { Target::Process(c.proc.clone()) });
    }
    if thread {
        return proc::find_thread(pid).filter(|t| t.group().tgid == c.proc.pid).map(Target::Thread);
    }
    if gettime && pid == c.thread.tid() {
        return Some(Target::Process(c.proc.clone()));
    }
    proc::find(pid).map(Target::Process)
}

/// clock_gettime on a CPU clock.
pub(crate) fn cpu_clock_ns(c: &SysCtx<'_>, pid: u32, thread: bool, which: u8) -> Result<u64, i64> {
    let t = clock_target(c, pid, thread, true).ok_or(-EINVAL)?;
    Ok(sample(t.times(), which))
}

/// clock_getres on a CPU clock (after the same check as clock_gettime).
pub(crate) fn cpu_clock_res(c: &SysCtx<'_>, pid: u32, thread: bool, which: u8) -> Result<u64, i64> {
    clock_target(c, pid, thread, true).ok_or(-EINVAL)?;
    Ok(if which == CPUCLOCK_SCHED { 1 } else { 1_000_000 })
}

/// clock_nanosleep on a CPU clock (`posix_cpu_nsleep` :1592): the caller's own thread clock is EINVAL (it
/// could never advance while the caller sleeps); another thread's or a process's clock sleeps until that CPU
/// time reaches the deadline, ending early when the caller is killed (EINTR, the CPU time left in `rmtp` for a
/// relative sleep) or the target is gone (0). Waits in slices (no CPU-time timer in V2: the next slice is the
/// time left spread over the online CPUs, 1 ms to 100 ms).
pub(crate) fn cpu_clock_sleep(c: &SysCtx<'_>, pid: u32, thread: bool, which: u8, flags: u64, ns: u64, rmtp: u64) -> i64 {
    if thread && (pid == 0 || pid == c.thread.tid()) {
        return -EINVAL;
    }
    let Some(t) = clock_target(c, pid, thread, false) else { return -EINVAL };
    let abs = flags & TIMER_ABSTIME != 0;
    let now = sample(t.times(), which);
    let deadline = if abs { ns } else { now.saturating_add(ns) };
    let own = matches!(&t, Target::Process(p) if p.pid == c.proc.pid);
    loop {
        let cur = sample(t.times(), which);
        if cur >= deadline || (!own && t.gone()) {
            return 0;
        }
        let left = deadline - cur;
        let slice = (left / sched::online_cpus().max(1) as u64).clamp(1_000_000, 100_000_000);
        if c.proc.sleep_interruptible(slice) {
            if !abs && rmtp != 0 {
                let left = deadline.saturating_sub(sample(t.times(), which));
                let _ = c.proc.mm.copy_to_user(rmtp, &uapi::Timespec::from_ns(left).to_bytes());
            }
            return -EINTR;
        }
    }
}

/// `struct tms` (four clock_t).
pub(crate) fn tms_bytes(own: CpuTimes, children: CpuTimes) -> [u8; 32] {
    let mut b = [0u8; 32];
    for (i, ns) in [own.user_ns, own.system_ns, children.user_ns, children.system_ns].into_iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&cputime::clock_t(ns).to_le_bytes());
    }
    b
}

fn sys_times(c: &mut SysCtx<'_>) -> i64 {
    let tbuf = c.args[0];
    if tbuf != 0 && c.proc.mm.copy_to_user(tbuf, &tms_bytes(process_times(c.proc), children_times(c.proc))).is_err() {
        return -EFAULT;
    }
    cputime::clock_t(time::now_ns()) as i64
}
crate::syscall_nr!(crate::sched_sys::nr::TIMES, "times", sys_times);

/// `struct rusage` (144 bytes): two timevals, then 14 longs.
pub(crate) fn rusage_bytes(t: CpuTimes, maxrss_kb: u64) -> [u8; uapi::ab::wait::RUSAGE_SIZE] {
    let mut b = [0u8; uapi::ab::wait::RUSAGE_SIZE];
    let tv = |ns: u64| ((ns / NSEC), (ns % NSEC) / 1000);
    let (us, uu) = tv(t.user_ns);
    let (ss, su) = tv(t.system_ns);
    for (i, v) in [us, uu, ss, su].into_iter().enumerate() {
        b[i * 8..i * 8 + 8].copy_from_slice(&v.to_le_bytes());
    }
    // ru_maxrss at 32, ru_nvcsw at 128, ru_nivcsw at 136.
    b[32..40].copy_from_slice(&maxrss_kb.to_le_bytes());
    b[128..136].copy_from_slice(&t.nvcsw.to_le_bytes());
    b[136..144].copy_from_slice(&t.nivcsw.to_le_bytes());
    b
}

fn sys_getrusage(c: &mut SysCtx<'_>) -> i64 {
    let who = c.args[0] as i32;
    let rss_kb = || c.proc.maxrss_pages(&c.proc.mm) * 4;
    let (t, maxrss) = match who {
        RUSAGE_SELF => (process_times(c.proc), rss_kb()),
        RUSAGE_THREAD => (thread_times(c.thread), rss_kb()),
        RUSAGE_CHILDREN => (children_times(c.proc), c.proc.children_maxrss_pages() * 4),
        _ => return -EINVAL,
    };
    c.proc.mm.copy_to_user(c.args[1], &rusage_bytes(t, maxrss)).map(|_| 0).unwrap_or(-EFAULT)
}
crate::syscall_nr!(crate::sched_sys::nr::GETRUSAGE, "getrusage", sys_getrusage);

#[cfg(test)]
mod tests {
    use super::*;

    fn u64_at(b: &[u8], off: usize) -> u64 {
        u64::from_le_bytes(b[off..off + 8].try_into().unwrap())
    }

    #[test]
    fn rusage_layout_as_linux() {
        let t = CpuTimes { user_ns: 3_500_000_123, system_ns: 999_999, nvcsw: 7, nivcsw: 9 };
        let b = rusage_bytes(t, 1234);
        assert_eq!(b.len(), 144);
        assert_eq!((u64_at(&b, 0), u64_at(&b, 8)), (3, 500_000), "ru_utime");
        assert_eq!((u64_at(&b, 16), u64_at(&b, 24)), (0, 999), "ru_stime");
        assert_eq!(u64_at(&b, 32), 1234, "ru_maxrss");
        for off in (40..128).step_by(8) {
            assert_eq!(u64_at(&b, off), 0, "field at {off}");
        }
        assert_eq!((u64_at(&b, 128), u64_at(&b, 136)), (7, 9), "ru_nvcsw, ru_nivcsw");
    }

    #[test]
    fn tms_in_clock_ticks() {
        let own = CpuTimes { user_ns: 1_234_000_000, system_ns: 19_999_999, ..CpuTimes::ZERO };
        let ch = CpuTimes { user_ns: 10_000_000, system_ns: 0, ..CpuTimes::ZERO };
        let b = tms_bytes(own, ch);
        assert_eq!([u64_at(&b, 0), u64_at(&b, 8), u64_at(&b, 16), u64_at(&b, 24)], [123, 1, 1, 0]);
    }

    #[test]
    fn samples_by_kind() {
        let t = CpuTimes { user_ns: 5, system_ns: 7, ..CpuTimes::ZERO };
        assert_eq!(sample(t, CPUCLOCK_PROF), 12);
        assert_eq!(sample(t, CPUCLOCK_VIRT), 5);
        assert_eq!(sample(t, CPUCLOCK_SCHED), 12);
    }
}
