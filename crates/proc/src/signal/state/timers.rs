//! Interval timers (setitimer/alarm) and POSIX timers (timer_create & co.) as part of the group's signal state,
//! where Linux keeps them too (`signal_struct`, under the siglock): linux-6.6/kernel/time/itimer.c,
//! kernel/time/posix-timers.c (`posix_timer_fn` :280, `common_timer_get` :620, `common_timer_set` :816),
//! kernel/time/posix-cpu-timers.c (`set_process_cpu_timer` :1461, `check_cpu_itimer` :947) and kernel/signal.c
//! (`dequeue_signal`'s SIGALRM re-arm :664, `posixtimer_rearm`, `send_sigqueue` :1955).
//!
//! Pure: the clocks come in as arguments (`now` is the monotonic clock, CPU clocks are sampled by the caller
//! outside the lock and passed as `Samples`), so host tests drive it; `crate::itimer` runs it.
//!
//! The Linux rules kept exactly:
//! - ITIMER_REAL fires once; a periodic one is re-armed only when its SIGALRM is dequeued (any SIGALRM), so a
//!   blocked or ignored SIGALRM pauses it and getitimer shows 0 meanwhile;
//! - ITIMER_VIRTUAL/PROF are absolute CPU-time expiries (one tick added at arming, `expires += incr` on firing);
//! - a POSIX timer has one preallocated siginfo: while it is queued, a further expiry only counts into its
//!   si_overrun; a periodic timer whose signal was queued is re-armed when that signal is dequeued
//!   (`it_requeue_pending` matching `si_sys_private`), counting the periods missed into `it_overrun`, and the
//!   delivered siginfo carries them (`it_overrun_last`, timer_getoverrun); an ignored signal re-arms at once
//!   (at least one tick ahead, `posix_timer_fn`'s throttle); timer_settime invalidates a queued signal's
//!   re-arm (`it_requeue_pending + 2`).

use super::*;
use sched::cputime::CpuTimes;

/// One tick of CachyOS's HZ=1000 (`TICK_NSEC`): what set/getitimer add or report for CPU itimers.
pub const TICK_NSEC: u64 = 1_000_000;
const REQUEUE_PENDING: u32 = 1;
/// Linux's CPUCLOCK kinds.
pub const CPU_PROF: u8 = 0;
pub const CPU_VIRT: u8 = 1;
pub const CPU_SCHED: u8 = 2;

/// What a POSIX timer counts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TClock {
    /// The monotonic clock (MONOTONIC, BOOTTIME; REALTIME/TAI deadlines are turned into monotonic ones when
    /// the timer is set: V2's wall clock never jumps).
    Mono,
    /// CPU time of this group's thread `key`.
    ThreadCpu { key: u64, which: u8 },
    /// CPU time of process `pid` (0: this group).
    GroupCpu { pid: u32, which: u8 },
}

/// Where a POSIX timer's signal goes (`it_sigev_notify` + `it_pid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    /// SIGEV_NONE: no signal (gettime still counts).
    None,
    /// SIGEV_SIGNAL / SIGEV_THREAD: the group.
    Group,
    /// SIGEV_THREAD_ID: this thread of the group.
    Thread(u64),
}

#[derive(Clone, Copy, Debug)]
pub struct PosixTimer {
    pub clock: TClock,
    pub notify: Notify,
    pub signo: u32,
    /// sigev_value.
    pub value: u64,
    /// The expiry in the clock's own nanoseconds (kept while disarmed: `hrtimer_get_expires`).
    pub expires: u64,
    pub interval: u64,
    /// `it_active`: armed and waiting to fire.
    pub active: bool,
    pub overrun: i64,
    pub overrun_last: i64,
    pub requeue: u32,
    /// The preallocated siginfo is on a pending list.
    pub queued: bool,
    /// The clock's value when last looked at (CPU clocks re-arm from it on dequeue).
    pub seen: u64,
    /// A CLOCK_REALTIME/TAI timer (counted on the monotonic clock; absolute times are wall times).
    pub wall: bool,
}

impl PosixTimer {
    pub fn new(clock: TClock, notify: Notify, signo: u32, value: u64) -> PosixTimer {
        PosixTimer { clock, notify, signo, value, expires: 0, interval: 0, active: false, overrun: -1, overrun_last: 0, requeue: 0, queued: false, seen: 0, wall: false }
    }
}

/// ITIMER_REAL (`real_timer` + `it_real_incr`).
#[derive(Clone, Copy, Debug, Default)]
pub struct RealTimer {
    pub expires: u64,
    pub active: bool,
    pub incr: u64,
}

/// ITIMER_VIRTUAL / ITIMER_PROF (`struct cpu_itimer`): an absolute CPU time, 0 = off.
#[derive(Clone, Copy, Debug, Default)]
pub struct CpuItimer {
    pub expires: u64,
    pub incr: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Timers {
    pub real: RealTimer,
    pub virt: CpuItimer,
    pub prof: CpuItimer,
    pub posix: BTreeMap<i32, PosixTimer>,
    /// `next_posix_timer_id` (kept across exec; a new group starts at 0).
    pub next_id: i32,
}

/// CPU clock values the caller sampled outside the lock (see `Signals::timer_needs`).
#[derive(Clone, Debug, Default)]
pub struct Samples {
    pub own: Option<CpuTimes>,
    pub threads: Vec<(u64, CpuTimes)>,
    pub others: Vec<(u32, CpuTimes)>,
}

/// What the CPU clocks of a pass need sampled: the group itself, these threads (key, kernel thread), these
/// other processes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Needs {
    pub own: bool,
    pub threads: Vec<(u64, u64)>,
    pub others: Vec<u32>,
}

/// A clock sample of kind `which` (PROF = user + system, VIRT = user, SCHED = all run time).
pub fn pick(t: &CpuTimes, which: u8) -> u64 {
    if which == CPU_VIRT { t.user_ns } else { t.total_ns() }
}

impl Samples {
    pub fn get(&self, c: TClock, now: u64) -> Option<u64> {
        match c {
            TClock::Mono => Some(now),
            TClock::GroupCpu { pid: 0, which } => self.own.as_ref().map(|t| pick(t, which)),
            TClock::GroupCpu { pid, which } => self.others.iter().find(|(p, _)| *p == pid).map(|(_, t)| pick(t, which)),
            TClock::ThreadCpu { key, which } => self.threads.iter().find(|(k, _)| *k == key).map(|(_, t)| pick(t, which)),
        }
    }
}

/// `hrtimer_forward(timer, now, interval)`: the first `expires + k * interval` after `now` and k (0 when
/// `now` is before `expires`). Pure.
pub fn forward(expires: u64, now: u64, interval: u64) -> (u64, u64) {
    if now < expires || interval == 0 {
        return (expires, 0);
    }
    let k = (now - expires) / interval + 1;
    (expires.saturating_add(k.saturating_mul(interval)), k)
}

/// `timer_overrun_to_int`.
fn overrun_int(last: i64, base: i32) -> i32 {
    let sum = last.saturating_add(base as i64);
    if sum > i32::MAX as i64 { i32::MAX } else { sum as i32 }
}

/// The SI_TIMER siginfo of timer `id` (`_timer`: si_tid 16, si_overrun 20, si_value 24, `si_sys_private` 32,
/// which the dequeue zeroes before a program sees it).
pub fn timer_info(signo: u32, id: i32, value: u64, sys_private: u32) -> SigInfo {
    let mut i = SigInfo::new(signo, SI_TIMER);
    i.0[16..20].copy_from_slice(&(id as u32).to_le_bytes());
    i.0[24..32].copy_from_slice(&value.to_le_bytes());
    i.0[32..36].copy_from_slice(&sys_private.to_le_bytes());
    i
}

/// What firing a POSIX timer asks the signal state to do.
enum Event {
    Queued,
    Ignored,
    /// The target thread is gone: the timer just stops (`send_sigqueue` < 0).
    Lost,
}

impl Timers {
    /// Something is armed or can re-arm (the service keeps looking at the group).
    pub fn configured(&self) -> bool {
        self.real.active || self.real.incr != 0 || self.virt.expires != 0 || self.prof.expires != 0 || !self.posix.is_empty()
    }

    /// `posix_timer_add`: the next free id (wraps within the positive ints); None when all are taken.
    pub fn alloc_id(&mut self) -> Option<i32> {
        for _ in 0..=self.posix.len() {
            let id = self.next_id;
            self.next_id = ((id as u32).wrapping_add(1) & i32::MAX as u32) as i32;
            if !self.posix.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }

    /// `itimer_get_remtime`: the time left of ITIMER_REAL (1 µs for an expired one still armed, 0 disarmed).
    pub fn real_get(&self, now: u64) -> (u64, u64) {
        let v = match self.real {
            RealTimer { active: false, .. } => 0,
            RealTimer { expires, .. } if expires > now => expires - now,
            _ => 1_000,
        };
        (v, self.real.incr)
    }

    /// `do_setitimer(ITIMER_REAL)`: returns the old (value, interval).
    pub fn real_set(&mut self, now: u64, value: u64, interval: u64) -> (u64, u64) {
        let old = self.real_get(now);
        self.real.active = false;
        if value != 0 {
            self.real.incr = interval;
            self.real.expires = now.saturating_add(value);
            self.real.active = true;
        } else {
            self.real.incr = 0;
        }
        old
    }

    fn cpu_itimer(&mut self, which: u8) -> &mut CpuItimer {
        if which == CPU_VIRT { &mut self.virt } else { &mut self.prof }
    }

    /// `get_cpu_itimer` with the group's sample `t` of that kind.
    pub fn cpu_get(&mut self, which: u8, t: u64) -> (u64, u64) {
        let it = *self.cpu_itimer(which);
        let v = match it.expires {
            0 => 0,
            e if e < t => TICK_NSEC,
            e => e - t,
        };
        (v, it.incr)
    }

    /// `set_cpu_itimer` with the group's sample `t`: returns the old (value, interval).
    pub fn cpu_set(&mut self, which: u8, t: u64, value: u64, interval: u64) -> (u64, u64) {
        let it = self.cpu_itimer(which);
        let mut oval = it.expires;
        let ointerval = it.incr;
        let mut nval = value;
        if oval != 0 || nval != 0 {
            if nval > 0 {
                nval = nval.saturating_add(TICK_NSEC);
            }
            if oval != 0 {
                oval = if oval <= t { TICK_NSEC } else { oval - t };
            }
            if nval != 0 {
                nval = nval.saturating_add(t);
            }
        }
        it.expires = nval;
        it.incr = interval;
        (oval, ointerval)
    }

    /// `common_timer_get` for timer `id` with its clock reading `now`: (value, interval), forwarding a periodic
    /// timer whose signal is pending (or a SIGEV_NONE one) and counting into its overrun.
    pub fn posix_get(&mut self, id: i32, now: u64) -> Option<(u64, u64)> {
        let t = self.posix.get_mut(&id)?;
        let sig_none = t.notify == Notify::None;
        let iv = t.interval;
        if iv == 0 && !t.active && !sig_none {
            return Some((0, 0));
        }
        if iv != 0 && (t.requeue & REQUEUE_PENDING != 0 || sig_none) {
            let (e, k) = forward(t.expires, now, iv);
            t.expires = e;
            t.overrun = t.overrun.saturating_add(k as i64);
        }
        let value = if t.expires > now {
            t.expires - now
        } else if sig_none {
            0
        } else {
            1
        };
        Some((value, iv))
    }

    /// `common_timer_set`: `value` is the expiry already in the clock's own time (0 = disarm); returns the
    /// old setting as `posix_get` reads it.
    pub fn posix_set(&mut self, id: i32, now: u64, expires: u64, interval: u64) -> Option<(u64, u64)> {
        let old = self.posix_get(id, now)?;
        let t = self.posix.get_mut(&id)?;
        t.active = false;
        t.interval = if expires != 0 { interval } else { 0 };
        t.requeue = t.requeue.wrapping_add(2) & !REQUEUE_PENDING;
        t.overrun_last = 0;
        t.overrun = -1;
        if expires == 0 {
            return Some(old);
        }
        t.expires = expires;
        t.seen = now;
        t.active = t.notify != Notify::None;
        Some(old)
    }

    /// `timer_getoverrun`.
    pub fn posix_overrun(&self, id: i32) -> Option<i32> {
        self.posix.get(&id).map(|t| overrun_int(t.overrun_last, 0))
    }
}

impl Signals {
    /// `exit_itimers` at execve: the POSIX timers go (a queued signal of theirs stays queued, as Linux's
    /// `sigqueue_free`); the interval timers stay.
    pub fn drop_posix_timers(&mut self) {
        self.timers.posix.clear();
    }

    /// The CPU clocks the armed timers of this group need sampled.
    pub fn timer_needs(&self) -> Needs {
        let mut n = Needs { own: self.timers.virt.expires != 0 || self.timers.prof.expires != 0, ..Needs::default() };
        for t in self.timers.posix.values().filter(|t| t.active || t.queued || t.interval != 0) {
            match t.clock {
                TClock::Mono => {}
                TClock::GroupCpu { pid: 0, .. } => n.own = true,
                TClock::GroupCpu { pid, .. } => {
                    if !n.others.contains(&pid) {
                        n.others.push(pid);
                    }
                }
                TClock::ThreadCpu { key, .. } => {
                    if let Some(th) = self.threads.get(&key)
                        && !n.threads.iter().any(|(k, _)| *k == key)
                    {
                        n.threads.push((key, th.kthread()));
                    }
                }
            }
        }
        n
    }

    /// One pass of the timer service: fires what is due at `now` (and the CPU clocks in `smp`), re-arms what
    /// re-arms at once, and returns the sends (signal + outcome, for `Group::after_send`) and when to look
    /// again: the earliest monotonic expiry and the least CPU time left over all armed CPU timers.
    pub fn fire_timers(&mut self, now: u64, smp: &Samples, limit: u64) -> (Vec<(u32, Outcome)>, Option<u64>, Option<u64>) {
        let mut out = Vec::new();
        let mut wall: Option<u64> = None;
        let mut cpu: Option<u64> = None;
        let min = |a: &mut Option<u64>, v: u64| *a = Some(a.map_or(v, |x| x.min(v)));
        // ITIMER_REAL (`it_real_fn`): SIGALRM to the group, re-armed when it is dequeued.
        if self.timers.real.active {
            if self.timers.real.expires <= now {
                self.timers.real.active = false;
                if let Ok(o) = self.send(SigInfo::kernel(SIGALRM), To::Group, false, limit) {
                    out.push((SIGALRM, o));
                }
            } else {
                min(&mut wall, self.timers.real.expires);
            }
        }
        // ITIMER_VIRTUAL / ITIMER_PROF (`check_cpu_itimer`).
        if let Some(own) = smp.own {
            for (which, sig) in [(CPU_VIRT, SIGVTALRM), (CPU_PROF, SIGPROF)] {
                let cur = pick(&own, which);
                let it = self.timers.cpu_itimer(which);
                if it.expires == 0 {
                    continue;
                }
                if cur >= it.expires {
                    it.expires = if it.incr != 0 { it.expires.saturating_add(it.incr) } else { 0 };
                    if let Ok(o) = self.send(SigInfo::kernel(sig), To::Group, false, limit) {
                        out.push((sig, o));
                    }
                }
                let it = self.timers.cpu_itimer(which);
                if it.expires != 0 {
                    min(&mut cpu, it.expires.saturating_sub(cur));
                }
            }
        }
        // POSIX timers (`posix_timer_fn`).
        let ids: Vec<i32> = self.timers.posix.iter().filter(|(_, t)| t.active).map(|(id, _)| *id).collect();
        for id in ids {
            let Some(t) = self.timers.posix.get(&id).copied() else { continue };
            let Some(cur) = smp.get(t.clock, now) else { continue };
            if let Some(x) = self.timers.posix.get_mut(&id) {
                x.seen = cur;
            }
            if t.expires > cur {
                match t.clock {
                    TClock::Mono => min(&mut wall, t.expires),
                    _ => min(&mut cpu, t.expires - cur),
                }
                continue;
            }
            let mut si_private = 0;
            if let Some(x) = self.timers.posix.get_mut(&id) {
                x.active = false;
                if x.interval != 0 {
                    x.requeue = x.requeue.wrapping_add(1);
                    si_private = x.requeue;
                }
            }
            match self.timer_event(id, &t, si_private, &mut out) {
                Event::Ignored if t.interval != 0 => {
                    // A handler may come later: keep it running, at least a tick apart (`posix_timer_fn`).
                    let at = if t.interval < TICK_NSEC { cur.saturating_add(TICK_NSEC) } else { cur };
                    if let Some(x) = self.timers.posix.get_mut(&id) {
                        let (e, k) = forward(x.expires, at, x.interval);
                        x.expires = e;
                        x.overrun = x.overrun.saturating_add(k as i64);
                        x.requeue = x.requeue.wrapping_add(1);
                        x.active = true;
                        match x.clock {
                            TClock::Mono => min(&mut wall, e),
                            _ => min(&mut cpu, e.saturating_sub(cur)),
                        }
                    }
                }
                _ => {}
            }
        }
        (out, wall, cpu)
    }

    /// `posix_timer_event` + `send_sigqueue`: the timer's one siginfo goes to its target unless it is queued
    /// already (then its si_overrun counts up).
    fn timer_event(&mut self, id: i32, t: &PosixTimer, si_private: u32, out: &mut Vec<(u32, Outcome)>) -> Event {
        let to = match t.notify {
            Notify::None => return Event::Ignored,
            Notify::Group => To::Group,
            Notify::Thread(k) => To::Thread(k),
        };
        let key = match to {
            To::Thread(k) if self.threads.contains_key(&k) => k,
            To::Thread(_) => return Event::Lost,
            To::Group => self.main_key(),
        };
        let mut o = Outcome { sent: Sent::Dropped, wake: Wake::default(), continued: false };
        if !self.prepare(t.signo, key, false, &mut o) {
            return Event::Ignored;
        }
        if t.queued {
            self.bump_queued_overrun(id);
            return Event::Queued;
        }
        match self.queue_prealloc(timer_info(t.signo, id, t.value, si_private), to, key, &mut o) {
            true => {
                if let Some(x) = self.timers.posix.get_mut(&id) {
                    x.queued = true;
                }
                out.push((t.signo, o));
                Event::Queued
            }
            false => Event::Lost,
        }
    }

    /// The timer's preallocated siginfo onto the target's list (no `legacy_queue`, no RLIMIT: it was counted
    /// when the timer was made). False when there is no memory for the entry.
    fn queue_prealloc(&mut self, info: SigInfo, to: To, key: u64, o: &mut Outcome) -> bool {
        let sig = info.signo();
        let shared = matches!(to, To::Group);
        let q = if shared {
            &mut self.shared
        } else {
            match self.threads.get_mut(&key) {
                Some(t) => &mut t.pending,
                None => return false,
            }
        };
        if q.q.try_reserve(1).is_err() {
            return false;
        }
        q.q.push(info);
        q.set |= bit(sig);
        self.queued += 1;
        o.sent = self.complete(sig, key, shared, &mut o.wake);
        true
    }

    /// `q->info.si_overrun++` on the queued siginfo of timer `id`.
    fn bump_queued_overrun(&mut self, id: i32) {
        let hit = |i: &SigInfo| i.code() == SI_TIMER && i.get32(16) as i32 == id;
        let bump = |i: &mut SigInfo| {
            let n = (i.get32(20) as i32).saturating_add(1);
            i.0[20..24].copy_from_slice(&(n as u32).to_le_bytes());
        };
        if let Some(i) = self.shared.q.iter_mut().find(|i| hit(i)) {
            bump(i);
            return;
        }
        for t in self.threads.values_mut() {
            if let Some(i) = t.pending.q.iter_mut().find(|i| hit(i)) {
                bump(i);
                return;
            }
        }
    }

    /// `dequeue_signal`'s timer part for a signal just taken (`now`: the monotonic clock): a SIGALRM re-arms
    /// a periodic ITIMER_REAL; a timer's siginfo frees its slot and, when it is the current one of a periodic
    /// timer, re-arms it and carries the overrun (`posixtimer_rearm`). Returns whether the service must look
    /// again (a timer was re-armed).
    pub(crate) fn timer_dequeued(&mut self, info: &mut SigInfo, now: u64) -> bool {
        let mut rearmed = false;
        if info.signo() == SIGALRM && !self.timers.real.active && self.timers.real.incr != 0 {
            let r = &mut self.timers.real;
            r.expires = forward(r.expires, now, r.incr).0;
            r.active = true;
            rearmed = true;
        }
        if info.code() != SI_TIMER {
            return rearmed;
        }
        let id = info.get32(16) as i32;
        let private = info.get32(32);
        if let Some(t) = self.timers.posix.get_mut(&id) {
            t.queued = false;
            if private != 0 && t.interval != 0 && t.requeue == private {
                let cur = if t.clock == TClock::Mono { now.max(t.seen) } else { t.seen };
                let (e, k) = forward(t.expires, cur, t.interval);
                t.expires = e;
                t.overrun = t.overrun.saturating_add(k as i64);
                t.active = true;
                t.overrun_last = t.overrun;
                t.overrun = -1;
                t.requeue = t.requeue.wrapping_add(1);
                let base = info.get32(20) as i32;
                let n = overrun_int(t.overrun_last, base);
                info.0[20..24].copy_from_slice(&(n as u32).to_le_bytes());
                rearmed = true;
            }
        }
        if private != 0 {
            info.0[32..36].copy_from_slice(&[0; 4]);
        }
        rearmed
    }
}
