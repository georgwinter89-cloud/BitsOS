//! Interval timers (setitimer/alarm) and POSIX timers (timer_create & co.) as part of the group's signal state,
//! where Linux keeps them too (`signal_struct`, under the siglock): linux-7.3.1/kernel/time/itimer.c,
//! kernel/time/posix-timers.c (`posix_timer_queue_signal` :357, `__posixtimer_deliver_signal` :299,
//! `common_timer_get` :684, `common_timer_set` :887, `do_timer_settime` :927), kernel/time/posix-cpu-timers.c
//! (`set_process_cpu_timer` :1504, `check_cpu_itimer` :983) and kernel/signal.c (`dequeue_signal` :624 with
//! `posixtimer_rearm_itimer`, itimer.c:161; `posixtimer_send_sigqueue` :2045, `posixtimer_sig_ignore` :2160,
//! `posixtimer_sig_unignore` :2175).
//!
//! Pure: the clocks come in as arguments (`now` is the monotonic clock, CPU clocks are sampled by the caller
//! outside the lock and passed as `Samples`), so host tests drive it; `crate::itimer` runs it.
//!
//! The Linux rules kept exactly:
//! - ITIMER_REAL fires once; a periodic one is re-armed only when its SIGALRM is dequeued (any SIGALRM), so a
//!   blocked or ignored SIGALRM pauses it and getitimer shows 0 meanwhile;
//! - ITIMER_VIRTUAL/PROF are absolute CPU-time expiries (one tick added at arming, `expires += incr` on firing);
//! - a POSIX timer has one preallocated siginfo. An expiry stops the timer (a periodic one waits as
//!   REQUEUE_PENDING) and queues the siginfo unless it is queued already (nothing more then: 7.3.1 counts no
//!   overrun into a queued siginfo). Its delivery re-arms a periodic timer, counting the periods missed into
//!   `overrun` and carrying them in si_overrun (`overrun_last`, timer_getoverrun);
//! - every timer_settime and the re-arm itself bump `signal_seq`; a queued siginfo whose `sigqueue_seq` (taken at
//!   its last expiry) is older is dropped when it is dequeued, as is one of a deleted timer (the queue entry, not
//!   the siginfo, carries the timer's `tag`, so a new timer with the same id is not mistaken for it and a
//!   program's own SI_TIMER siginfo never passes for a timer's);
//! - a periodic timer whose signal is ignored is parked (`ignored`, Linux's `ignored_posix_timers`) instead of
//!   re-armed, and so is one whose queued siginfo is flushed because the signal became ignored; when the signal
//!   stops being ignored (sigaction from SIG_IGN) its siginfo is queued at once and the delivery re-arms it.
use super::*;
use sched::cputime::CpuTimes;

/// One tick of CachyOS's HZ=1000 (`TICK_NSEC`): what set/getitimer add or report for CPU itimers.
pub const TICK_NSEC: u64 = 1_000_000;
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
    /// POSIX_TIMER_ARMED: armed and waiting to fire.
    pub active: bool,
    /// POSIX_TIMER_REQUEUE_PENDING: a periodic timer that fired and waits for its signal's delivery to re-arm.
    pub requeue_pending: bool,
    pub overrun: i64,
    pub overrun_last: i64,
    /// `it_signal_seq`: bumped by timer_settime and by each re-arm.
    pub signal_seq: u32,
    /// `it_sigqueue_seq`: `signal_seq` at the last expiry that sent (or parked) the siginfo.
    pub sigqueue_seq: u32,
    /// `it_sig_periodic`: that expiry was of a periodic timer.
    pub sig_periodic: bool,
    /// The preallocated siginfo is on a pending list.
    pub queued: bool,
    /// Parked on the group's ignored timers (its signal was ignored).
    pub ignored: bool,
    /// The tag its queued siginfo's entry carries (`Queued::timer`; 0: none given yet).
    pub tag: u32,
    /// The clock's value when last looked at (CPU clocks re-arm from it on dequeue).
    pub seen: u64,
    /// A CLOCK_REALTIME/TAI timer (counted on the monotonic clock; absolute times are wall times).
    pub wall: bool,
}

impl PosixTimer {
    pub fn new(clock: TClock, notify: Notify, signo: u32, value: u64) -> PosixTimer {
        PosixTimer {
            clock,
            notify,
            signo,
            value,
            expires: 0,
            interval: 0,
            active: false,
            requeue_pending: false,
            overrun: -1,
            overrun_last: 0,
            signal_seq: 0,
            sigqueue_seq: 0,
            sig_periodic: false,
            queued: false,
            ignored: false,
            tag: 0,
            seen: 0,
            wall: false,
        }
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
    /// PR_TIMER_CREATE_RESTORE_IDS: timer_create takes the id it is given (off again at exec).
    pub restore_ids: bool,
    /// The last `PosixTimer::tag` given out.
    pub last_tag: u32,
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

/// The SI_TIMER siginfo of timer `id` (`_timer`: si_tid 16, si_overrun 20, si_value 24; si_sys_private stays
/// 0: the timer's tag goes into the queue entry, not the bytes).
pub fn timer_info(signo: u32, id: i32, value: u64) -> SigInfo {
    let mut i = SigInfo::new(signo, SI_TIMER);
    i.0[16..20].copy_from_slice(&(id as u32).to_le_bytes());
    i.0[24..32].copy_from_slice(&value.to_le_bytes());
    i
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
        if iv != 0 && !t.active {
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
        // `do_timer_settime`: a queued siginfo of the old setting is no longer delivered.
        t.signal_seq = t.signal_seq.wrapping_add(1);
        t.active = false;
        t.requeue_pending = false;
        t.interval = if expires != 0 { interval } else { 0 };
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

    /// The timer of a preallocated siginfo: `id` from its si_tid (the kernel wrote it), `tag` from the queue
    /// entry; None when that timer is gone.
    fn by_info(&mut self, info: &SigInfo, tag: u32) -> Option<&mut PosixTimer> {
        let id = info.get32(16) as i32;
        self.posix.get_mut(&id).filter(|t| tag != 0 && t.tag == tag)
    }

    fn tag_of(&mut self, id: i32) -> u32 {
        let next = match self.last_tag.wrapping_add(1) {
            0 => 1,
            n => n,
        };
        match self.posix.get_mut(&id) {
            Some(t) if t.tag != 0 => t.tag,
            Some(t) => {
                self.last_tag = next;
                t.tag = next;
                next
            }
            None => 0,
        }
    }
}

impl Signals {
    /// `exit_itimers` at execve: the POSIX timers go (a queued signal of theirs stays queued, as Linux's
    /// `sigqueue_free`); the interval timers stay.
    pub fn drop_posix_timers(&mut self) {
        self.timers.posix.clear();
        self.timers.restore_ids = false;
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
        let mut out = core::mem::take(&mut self.timer_sends);
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
            if let Some(x) = self.timers.posix.get_mut(&id) {
                x.active = false;
                x.requeue_pending = x.interval != 0;
            }
            self.timer_event(id, &mut out);
        }
        (out, wall, cpu)
    }

    /// `common_timer_set`'s "timer was already expired, queue the signal" (linux-7.3.1/kernel/time/posix-timers.c
    /// :887): a monotonic-clock timer just set to an expiry not after `now` fires inside timer_settime, so its
    /// signal is pending when the call returns. The sends' wake-ups go to the timer service as sigaction's do
    /// (`timers_unignored`). SIGEV_NONE timers never queue; CPU-clock timers are left to the service.
    pub fn posix_fire_if_due(&mut self, id: i32, now: u64) {
        let Some(t) = self.timers.posix.get_mut(&id) else { return };
        if !t.active || t.clock != TClock::Mono || t.notify == Notify::None || t.expires > now {
            return;
        }
        t.active = false;
        t.requeue_pending = t.interval != 0;
        t.seen = now;
        let mut out = core::mem::take(&mut self.timer_sends);
        self.timer_event(id, &mut out);
        if !out.is_empty() {
            crate::itimer::nudge();
        }
        self.timer_sends = out;
    }

    /// `posixtimer_send_sigqueue`: the timer's one siginfo goes to its target unless it is queued already; an
    /// ignored signal parks a periodic timer (and unparks a one-shot one: its old siginfo is void). A target
    /// thread that is gone stops the timer for good.
    fn timer_event(&mut self, id: i32, out: &mut Vec<(u32, Outcome)>) {
        let Some(t) = self.timers.posix.get(&id).copied() else { return };
        let to = match t.notify {
            Notify::None => return,
            Notify::Group => To::Group,
            Notify::Thread(k) => To::Thread(k),
        };
        let key = match to {
            To::Thread(k) if self.threads.contains_key(&k) => k,
            To::Thread(_) => return,
            To::Group => self.main_key(),
        };
        if let Some(x) = self.timers.posix.get_mut(&id) {
            x.sigqueue_seq = x.signal_seq;
            x.sig_periodic = x.requeue_pending;
        }
        let mut o = Outcome { sent: Sent::Dropped, wake: Wake::default(), continued: false };
        if !self.prepare(t.signo, key, false, &mut o) {
            if let Some(x) = self.timers.posix.get_mut(&id)
                && !x.queued
            {
                x.ignored = x.sig_periodic;
            }
            return;
        }
        if t.queued {
            return;
        }
        if let Some(x) = self.timers.posix.get_mut(&id) {
            x.ignored = false;
        }
        self.queue_timer(id, to, key, o, out);
    }

    /// The timer's siginfo onto `to`'s list (`posixtimer_queue_sigqueue`, no `prepare_signal`).
    fn queue_timer(&mut self, id: i32, to: To, key: u64, mut o: Outcome, out: &mut Vec<(u32, Outcome)>) {
        let tag = self.timers.tag_of(id);
        let Some(t) = self.timers.posix.get(&id).copied() else { return };
        if self.queue_prealloc(timer_info(t.signo, id, t.value), tag, to, key, &mut o) {
            if let Some(x) = self.timers.posix.get_mut(&id) {
                x.queued = true;
            }
            if out.try_reserve(1).is_ok() {
                out.push((t.signo, o));
            }
        }
    }

    /// `posixtimer_sig_unignore` (from `sigaction`, when `sig` stops being ignored): the parked timers of it queue
    /// their siginfo now, so the signal is pending when sigaction returns (the delivery re-arms them). The sends'
    /// wake-ups and a fatal outcome are the timer service's to carry out (`fire_timers` hands them over first):
    /// sigaction's caller has no `Group::after_send`.
    pub(crate) fn timers_unignored(&mut self, sig: u32) {
        let mut out = core::mem::take(&mut self.timer_sends);
        let ids: Vec<i32> = self.timers.posix.iter().filter(|(_, t)| t.ignored && t.signo == sig).map(|(id, _)| *id).collect();
        for id in ids {
            let Some(t) = self.timers.posix.get_mut(&id) else { continue };
            t.ignored = false;
            if t.queued {
                continue;
            }
            let (to, key) = match t.notify {
                Notify::Group if !self.exiting => (To::Group, self.main_key()),
                Notify::Thread(k) if self.threads.contains_key(&k) => (To::Thread(k), k),
                _ => continue,
            };
            let o = Outcome { sent: Sent::Dropped, wake: Wake::default(), continued: false };
            self.queue_timer(id, to, key, o, &mut out);
        }
        if !out.is_empty() {
            crate::itimer::nudge();
        }
        self.timer_sends = out;
    }

    /// `sigqueue_free_ignored` for every timer siginfo of `mask` a flush is about to remove: it is no longer
    /// queued, and the timer is parked when that siginfo came from a periodic expiry.
    pub(crate) fn timers_flushed(&mut self, mask: u64) {
        for t in self.timers.posix.values_mut().filter(|t| t.queued && bit_checked(t.signo) & mask != 0) {
            t.queued = false;
            t.ignored = t.sig_periodic;
        }
    }

    /// A thread's own queue went with it: its timers' siginfos are no longer queued.
    pub(crate) fn timers_lost_thread(&mut self, key: u64) {
        for t in self.timers.posix.values_mut().filter(|t| t.notify == Notify::Thread(key)) {
            t.queued = false;
        }
    }

    /// The timer's preallocated siginfo onto the target's list (no `legacy_queue`, no RLIMIT: it was counted
    /// when the timer was made). False when there is no memory for the entry.
    fn queue_prealloc(&mut self, info: SigInfo, tag: u32, to: To, key: u64, o: &mut Outcome) -> bool {
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
        if !q.push(info, tag) {
            return false;
        }
        q.set |= bit(sig);
        self.queued += 1;
        o.sent = self.complete(sig, key, shared, &mut o.wake);
        true
    }

    /// `dequeue_signal`'s timer part for a signal just taken (`now`: the monotonic clock): a SIGALRM re-arms
    /// a periodic ITIMER_REAL; a timer's preallocated siginfo (`tag` not 0, Linux's `timer_sigq`) frees its slot
    /// and goes through `__posixtimer_deliver_signal`: dropped when its timer is gone or was set again since
    /// that expiry, else delivered, a periodic timer re-armed with the missed periods counted into si_overrun.
    /// Any other siginfo, a program's SI_TIMER one too, is delivered as it is. Returns (deliver it, the
    /// service must look again).
    pub(crate) fn timer_dequeued(&mut self, info: &mut SigInfo, tag: u32, now: u64) -> (bool, bool) {
        let mut rearmed = false;
        if info.signo() == SIGALRM && !self.timers.real.active && self.timers.real.incr != 0 {
            let r = &mut self.timers.real;
            r.expires = forward(r.expires, now, r.incr).0;
            r.active = true;
            rearmed = true;
        }
        if tag == 0 {
            return (true, rearmed);
        }
        let deliver = match self.timers.by_info(info, tag) {
            None => false,
            Some(t) => {
                t.queued = false;
                if t.signal_seq != t.sigqueue_seq {
                    false
                } else {
                    if t.interval != 0 && t.requeue_pending {
                        let cur = if t.clock == TClock::Mono { now.max(t.seen) } else { t.seen };
                        let (e, k) = forward(t.expires, cur, t.interval);
                        t.expires = e;
                        t.overrun = t.overrun.saturating_add(k as i64);
                        t.active = true;
                        t.requeue_pending = false;
                        t.overrun_last = t.overrun;
                        t.overrun = -1;
                        t.signal_seq = t.signal_seq.wrapping_add(1);
                        let n = overrun_int(t.overrun_last, 0);
                        info.0[20..24].copy_from_slice(&(n as u32).to_le_bytes());
                        rearmed = true;
                    }
                    true
                }
            }
        };
        (deliver, rearmed)
    }
}
