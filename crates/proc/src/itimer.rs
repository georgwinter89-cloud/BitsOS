//! AB-6 fixer: the senders of SIGALRM, SIGVTALRM, SIGPROF and SI_TIMER signals — setitimer/getitimer/alarm
//! (linux-6.6/kernel/time/itimer.c) and POSIX timers (kernel/time/posix-timers.c, posix-cpu-timers.c). The
//! timers' state is part of the group's signal state (`signal::Timers`, under `proc.signals`, where Linux keeps
//! it under the siglock); this file is the service that fires them and the group's calls the syscall layer uses.
//!
//! The service is one kernel thread (`proc.timers`, Interactive: a timer signal is due at a time). It keeps the
//! groups that have a timer set (`ARMED`, weak references), and on each pass fires what is due in each, then
//! sleeps until the earliest monotonic expiry, or — for CPU-clock timers — a slice of the CPU time left spread
//! over the online CPUs (1 ms to 100 ms: CPU time cannot advance faster), at most a second (B4). Setting a timer
//! (and a dequeue that re-arms one) wakes it. Timing is the scheduler's sleep timer (an HrTimer, SC-4): not
//! judged in QEMU, on the Mini-PC list.
//!
//! Locks: `proc.timers.armed` > `proc.signals`. Sends are made under `proc.signals`, their outcome (kicks, a
//! fatal signal's end, signalfd polls) is finished outside it (`Group::after_send`).

use crate::group::Group;
use crate::signal::{CPU_PROF, CPU_VIRT, Needs, Notify, PosixTimer, Samples, TClock};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{SpinLock, WaitQueue};
use sched::cputime::CpuTimes;
use uapi::ab::timers::*;
use uapi::errno::*;
use vfs::Errno;

static ARMED: SpinLock<Vec<Weak<Group>>> = SpinLock::named("proc.timers.armed", Vec::new());
static WQ: WaitQueue = WaitQueue::new();
static GEN: AtomicU64 = AtomicU64::new(0);
static STARTED: AtomicBool = AtomicBool::new(false);
/// The longest the service sleeps without looking (B4).
const MAX_SLEEP_NS: u64 = 1_000_000_000;
const CPU_SLICE_MIN_NS: u64 = 1_000_000;
const CPU_SLICE_MAX_NS: u64 = 100_000_000;

klog::counter!(FIRED, "proc.timer_signals");
klog::counter!(PASSES, "proc.timer_passes");

/// Starts the service (`proc::init`). Without it no timer fires (logged).
pub(crate) fn start() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if sched::spawn("proc.timers", sched::Class::Interactive, service).is_err() {
        STARTED.store(false, Ordering::Release);
        klog::event!(Warn, "proc", "timers.no_thread"; "no thread for the timer service: setitimer/alarm/timer_create timers never fire");
    }
}

/// Tells the service to look again (a timer was set or re-armed). Any context that may take a spin lock.
pub(crate) fn nudge() {
    GEN.fetch_add(1, Ordering::SeqCst);
    WQ.wake_all();
}

/// Group `g` has a timer set: the service watches it (from now until it has none).
fn watch(g: &Arc<Group>) {
    {
        let mut a = ARMED.lock();
        if !g.timers_listed.load(Ordering::Acquire) && a.try_reserve(1).is_ok() {
            a.push(Arc::downgrade(g));
            g.timers_listed.store(true, Ordering::Release);
        }
    }
    nudge();
}

fn service() {
    klog::event!(Info, "proc", "timers.started");
    loop {
        let gen0 = GEN.load(Ordering::SeqCst);
        PASSES.inc();
        let groups: Vec<Arc<Group>> = ARMED.lock().iter().filter_map(Weak::upgrade).collect();
        let mut wake_at = time::now_ns().saturating_add(MAX_SLEEP_NS);
        for g in &groups {
            if let Some(at) = pass(g) {
                wake_at = wake_at.min(at);
            }
        }
        prune();
        drop(groups);
        let now = time::now_ns();
        if wake_at > now {
            WQ.wait_until_timeout(|| GEN.load(Ordering::SeqCst) != gen0, wake_at - now);
        }
    }
}

/// Drops the groups that ended or have no timer set any more (their Arcs are dropped outside the lock).
fn prune() {
    let mut gone: Vec<Arc<Group>> = Vec::new();
    let mut a = ARMED.lock();
    a.retain(|w| match w.upgrade() {
        None => false,
        Some(g) => {
            let keep = !g.is_finished() && g.signals.raw(|s| s.timers.configured());
            if !keep {
                g.timers_listed.store(false, Ordering::Release);
            }
            let _ = gone.try_reserve(1).map(|_| gone.push(g));
            keep
        }
    });
    drop(a);
    drop(gone);
}

/// The CPU clocks group `g`'s timers need, sampled with no lock held.
fn sample(g: &Group, n: &Needs) -> Samples {
    let mut s = Samples { own: n.own.then(|| g.cpu_times()), ..Samples::default() };
    for &(key, kthread) in &n.threads {
        if let Some(t) = sched::cputime::of_thread(kthread) {
            s.threads.push((key, t));
        }
    }
    for &pid in &n.others {
        if let Some(p) = crate::find(pid) {
            s.others.push((pid, p.cpu_times()));
        }
    }
    s
}

fn sigpending_limit(g: &Group) -> u64 {
    g.limits.get(uapi::ab::rlimit::RLIMIT_SIGPENDING).map_or(u64::MAX, |l| l.cur)
}

/// One group's pass: fire, finish the sends, and when to look again (absolute monotonic ns).
fn pass(g: &Arc<Group>) -> Option<u64> {
    if g.is_finished() {
        return None;
    }
    let needs = g.signals.raw(|s| s.timer_needs());
    let smp = sample(g, &needs);
    let now = time::now_ns();
    let limit = sigpending_limit(g);
    let (fired, wall, cpu) = g.signals.raw(|s| s.fire_timers(now, &smp, limit));
    for (sig, out) in fired {
        FIRED.inc();
        klog::event!(Debug, "proc", "timer.fired", pid = g.tgid, signal = uapi::ab::signal_names::name(sig));
        g.after_send(sig, out);
    }
    let cpu_at = cpu.map(|left| now.saturating_add((left / sched::online_cpus().max(1) as u64).clamp(CPU_SLICE_MIN_NS, CPU_SLICE_MAX_NS)));
    match (wall, cpu_at) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// A POSIX timer's clock as the syscall layer decoded it (REALTIME/TAI count as monotonic with the timer's
/// `wall` flag set, for absolute times).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockSpec {
    Mono,
    /// CPU time (`which` = PROF/VIRT/SCHED) of process `pid` or, with `thread`, of thread `pid` (0 = the caller).
    Cpu { pid: u32, thread: bool, which: u8 },
}

/// `struct sigevent` as timer_create reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SigEvent {
    pub value: u64,
    pub signo: i32,
    pub notify: i32,
    pub tid: i32,
}

impl SigEvent {
    pub fn from_bytes(b: &[u8; SIGEVENT_SIZE]) -> SigEvent {
        let w32 = |o: usize| i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        SigEvent { value: u64::from_le_bytes(b[0..8].try_into().unwrap_or([0; 8])), signo: w32(8), notify: w32(12), tid: w32(16) }
    }
}

impl Group {
    /// The calling thread's key in this group (its main thread for kernel callers).
    fn timer_caller(&self) -> (u64, u32) {
        match crate::task::current_thread().filter(|t| t.is_of(self)) {
            Some(t) => (t.sig_key(), t.tid()),
            None => (self.signals.raw(|s| s.main_key()), self.tgid),
        }
    }

    fn own_sample(&self, which: u8) -> u64 {
        crate::signal::pick(&self.cpu_times(), which)
    }

    /// `do_getitimer`: (value, interval) in ns. EINVAL for an unknown `which`.
    pub fn getitimer(&self, which: i32) -> Result<(u64, u64), Errno> {
        match which {
            ITIMER_REAL => Ok(self.signals.raw(|s| s.timers.real_get(time::now_ns()))),
            ITIMER_VIRTUAL | ITIMER_PROF => {
                let w = if which == ITIMER_VIRTUAL { CPU_VIRT } else { CPU_PROF };
                let t = self.own_sample(w);
                Ok(self.signals.raw(|s| s.timers.cpu_get(w, t)))
            }
            _ => Err(EINVAL),
        }
    }

    /// `do_setitimer`: sets `which` to `value` (0 disarms) with `interval`, returns the old setting.
    pub fn setitimer(self: &Arc<Self>, which: i32, value: u64, interval: u64) -> Result<(u64, u64), Errno> {
        let old = match which {
            ITIMER_REAL => self.signals.raw(|s| s.timers.real_set(time::now_ns(), value, interval)),
            ITIMER_VIRTUAL | ITIMER_PROF => {
                let w = if which == ITIMER_VIRTUAL { CPU_VIRT } else { CPU_PROF };
                let t = self.own_sample(w);
                self.signals.raw(|s| s.timers.cpu_set(w, t, value, interval))
            }
            _ => return Err(EINVAL),
        };
        klog::event!(Debug, "proc", "itimer.set", pid = self.tgid, which = which, value_ns = value, interval_ns = interval);
        if value != 0 {
            watch(self);
        } else {
            nudge();
        }
        Ok(old)
    }

    /// `alarm(seconds)` (`alarm_setitimer`): the old ITIMER_REAL in whole seconds, rounded up when under a
    /// second or by half a second or more (so a pending alarm never reads 0).
    pub fn alarm(self: &Arc<Self>, seconds: u32) -> u32 {
        let (old, _) = self.setitimer(ITIMER_REAL, seconds as u64 * 1_000_000_000, 0).unwrap_or((0, 0));
        let (sec, nsec) = (old / 1_000_000_000, old % 1_000_000_000);
        let up = (sec == 0 && nsec != 0) || nsec >= 500_000_000;
        (sec + up as u64).min(u32::MAX as u64) as u32
    }

    /// `do_timer_create` up to the id: RLIMIT_SIGPENDING for the timer's preallocated siginfo and the id
    /// (EAGAIN), then the sigevent (`good_sigevent`; EINVAL with the id used up anyway, as Linux). Returns the id
    /// and where its signals go: (id, notify, signo, sigev_value).
    pub fn timer_prepare(&self, ev: Option<SigEvent>) -> Result<(i32, Notify, u32, u64), (Option<i32>, Errno)> {
        let limit = sigpending_limit(self);
        let id = self.signals.raw(|s| {
            if s.queued() + s.timers.posix.len() as u64 >= limit {
                return Err(EAGAIN);
            }
            s.timers.alloc_id().ok_or(EAGAIN)
        });
        let id = id.map_err(|e| (None, e))?;
        let Some(e) = ev else { return Ok((id, Notify::Group, uapi::ab::signal::SIGALRM, id as u32 as u64)) };
        let notify = match e.notify {
            SIGEV_NONE => Notify::None,
            SIGEV_SIGNAL | SIGEV_THREAD => Notify::Group,
            n if n == SIGEV_SIGNAL | SIGEV_THREAD_ID => {
                let t = (e.tid > 0).then(|| crate::find_thread(e.tid as u32)).flatten();
                match t.filter(|t| t.group().tgid == self.tgid) {
                    Some(t) => Notify::Thread(t.sig_key()),
                    None => return Err((Some(id), EINVAL)),
                }
            }
            _ => return Err((Some(id), EINVAL)),
        };
        if notify != Notify::None && !(1..=crate::signal::NSIG as i32).contains(&e.signo) {
            return Err((Some(id), EINVAL));
        }
        Ok((id, notify, e.signo.max(0) as u32, e.value))
    }

    /// `kc->timer_create` (after the id reached the program): the clock's target (`pid_for_clock(clock,
    /// false)`: a thread clock names a thread of this group, a process clock a process by pid, 0 = the caller).
    pub fn timer_clock(&self, clock: ClockSpec) -> Result<TClock, Errno> {
        let (caller_key, caller_tid) = self.timer_caller();
        Ok(match clock {
            ClockSpec::Mono => TClock::Mono,
            ClockSpec::Cpu { pid, thread: true, which } => {
                let key = if pid == 0 || pid == caller_tid {
                    caller_key
                } else {
                    crate::find_thread(pid).filter(|t| t.group().tgid == self.tgid).ok_or(EINVAL)?.sig_key()
                };
                TClock::ThreadCpu { key, which }
            }
            ClockSpec::Cpu { pid, thread: false, which } => {
                if pid == 0 || pid == self.tgid {
                    TClock::GroupCpu { pid: 0, which }
                } else if crate::find(pid).is_some() {
                    TClock::GroupCpu { pid, which }
                } else {
                    return Err(EINVAL);
                }
            }
        })
    }

    /// Puts a prepared timer in place (after its id reached the program).
    pub fn timer_insert(&self, id: i32, t: PosixTimer) -> Result<(), Errno> {
        self.signals.raw(|s| {
            if s.timers.posix.contains_key(&id) {
                return Err(EAGAIN);
            }
            s.timers.posix.insert(id, t);
            Ok(())
        })?;
        klog::event!(Debug, "proc", "timer.created", pid = self.tgid, id = id);
        Ok(())
    }

    /// The current value of timer `id`'s clock (None: no such timer).
    fn timer_clock_now(&self, id: i32) -> Option<u64> {
        let c = self.signals.raw(|s| s.timers.posix.get(&id).map(|t| t.clock))?;
        let now = time::now_ns();
        Some(match c {
            TClock::Mono => now,
            TClock::GroupCpu { pid: 0, which } => self.own_sample(which),
            TClock::GroupCpu { pid, which } => crate::find(pid).map_or(0, |p| crate::signal::pick(&p.cpu_times(), which)),
            TClock::ThreadCpu { key, which } => {
                let k = self.signals.raw(|s| s.thread(key).map(|t| t.kthread()));
                k.and_then(sched::cputime::of_thread).map_or(0, |t: CpuTimes| crate::signal::pick(&t, which))
            }
        })
    }

    /// `timer_gettime`: (value, interval) in ns; EINVAL for an unknown id.
    pub fn timer_gettime(&self, id: i32) -> Result<(u64, u64), Errno> {
        let now = self.timer_clock_now(id).ok_or(EINVAL)?;
        self.signals.raw(|s| s.timers.posix_get(id, now)).ok_or(EINVAL)
    }

    /// `timer_settime`: `value` relative to the clock's now, or absolute with `abs` (in the clock's own time);
    /// returns the old setting. EINVAL for an unknown id.
    /// `real_now`: the wall clock now, for an absolute time on a CLOCK_REALTIME/TAI timer.
    pub fn timer_settime(self: &Arc<Self>, id: i32, abs: bool, value: u64, interval: u64, real_now: u64) -> Result<(u64, u64), Errno> {
        let now = self.timer_clock_now(id).ok_or(EINVAL)?;
        let wall = self.signals.raw(|s| s.timers.posix.get(&id).is_some_and(|t| t.wall));
        let expires = match (value, abs) {
            (0, _) => 0,
            // V2's wall clock never jumps: an absolute wall time becomes a monotonic one now.
            (v, true) if wall => now.saturating_add(v.saturating_sub(real_now)).max(1),
            // An absolute time already past fires at once (but never names "disarmed").
            (v, true) => v.max(1),
            (v, false) => now.saturating_add(v),
        };
        let old = self.signals.raw(|s| s.timers.posix_set(id, now, expires, interval)).ok_or(EINVAL)?;
        if expires != 0 {
            watch(self);
        }
        Ok(old)
    }

    /// `timer_getoverrun`.
    pub fn timer_getoverrun(&self, id: i32) -> Result<i32, Errno> {
        self.signals.raw(|s| s.timers.posix_overrun(id)).ok_or(EINVAL)
    }

    /// `timer_delete`: a queued signal of it stays queued (`sigqueue_free`).
    pub fn timer_delete(&self, id: i32) -> Result<(), Errno> {
        self.signals.raw(|s| s.timers.posix.remove(&id)).ok_or(EINVAL)?;
        nudge();
        Ok(())
    }
}

/// Timer signals sent so far (ktests).
pub fn fired_count() -> u64 {
    FIRED.get()
}
