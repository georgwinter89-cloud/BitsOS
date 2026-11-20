//! The sampling clock (linux-7.3.1/kernel/events/core.c `perf_swevent_hrtimer`): one `HrTimer` per CPU, armed on
//! its own CPU, running at the shortest period of the enabled sampling events there (at least 10 µs, as Linux).
//! Each expiry runs in hard-IRQ context: it looks at the code the timer interrupted (`hal::sampling`), adds the
//! time since the last expiry to every matching event, and writes a sample for each event whose period is
//! full. A CPU event matches on its CPU; a thread event on the expiries that find its thread running, so its
//! samples follow the thread's run time as Linux's per-task timer does (Linux starts and stops that timer at
//! every switch; V2 keeps the CPU's timer running while a thread event exists, a decision in the notes).
//!
//! Nothing here allocates or sleeps: the lists are rebuilt in thread context and swapped in under the IRQ-safe
//! lock; a timer is started on its CPU by a cross-CPU call.
//!
//! The cost is bounded as Linux bounds it: a CPU's timer runs no faster than `perf_event_max_sample_rate`
//! (Linux's `max_samples_per_tick` throttle; one timer serves all events of a CPU, so it bounds the interrupts,
//! not only the samples), the interrupt takes at most `perf_cpu_time_max_percent` of each 1 ms window (past it
//! the timer waits for the next window, as a throttled Linux event waits for the next tick), and a thread event
//! of an unprivileged caller, whose timer runs on every CPU here, gets 1/CPUs of that rate.

use super::event::{Event, Target, limit_hit};
use super::{SAMPLES, SAMPLES_SKIPPED, THROTTLED, TICK_NS, TICKS};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::sampling::Interrupted;
use ksync::IrqSpinLock;
use perf_abi::attr::flag;
use perf_abi::record::{self, Ident, Sample};
use perf_abi::*;
use time::HrTimer;

/// Linux's floor for an hrtimer period (`perf_swevent_start_hrtimer`).
pub(crate) const MIN_PERIOD_NS: u64 = 10_000;
/// The window the CPU-time share is judged in (a Linux tick at HZ 1000).
const WINDOW_NS: u64 = 1_000_000;
/// The callchain buffer: `perf_event_max_stack` up to this, plus the context marks.
const CHAIN_MAX: usize = 255;
const CHAIN_BUF: usize = CHAIN_MAX + 8;
/// Group members a sample's group read lists (more are left out of samples; read(2) lists all).
const MAX_GROUP_READ: usize = 16;
/// A sample record's buffer (header, fields, a group read, the chain).
const REC_BUF: usize = 8 + 10 * 8 + (3 + 3 * (MAX_GROUP_READ + 1)) * 8 + 8 + CHAIN_BUF * 8;
/// Kernel threads have no tid: theirs is the scheduler id with this bit (never a Linux tid, which stays below
/// 2^22).
pub(crate) const KTHREAD_TID: u32 = 0x8000_0000;

struct Sampler {
    timer: HrTimer,
    events: IrqSpinLock<Vec<Arc<Event>>>,
    /// The period it runs at (0: stopped).
    period: AtomicU64,
    /// The last expiry and the deadline it was armed for (ns).
    last: AtomicU64,
    deadline: AtomicU64,
    /// The current 1 ms window's start and the interrupt time spent in it (ns).
    window: AtomicU64,
    busy: AtomicU64,
    /// Expiries and their time on this CPU (ktests judge the bound per CPU).
    fires: AtomicU64,
    fire_ns: AtomicU64,
}

static SAMPLERS: ksync::Once<Vec<Sampler>> = ksync::Once::new();
/// Serialises list changes and starts (thread context).
static CHANGE: ksync::Mutex<()> = ksync::Mutex::named("perf.sampler_change", ());

fn samplers() -> &'static [Sampler] {
    SAMPLERS.get_or_init(|| {
        let n = hal::percpu::possible_cpus().max(hal::percpu::cpu_count());
        (0..n)
            .map(|c| Sampler {
                timer: HrTimer::new(fire, c),
                events: IrqSpinLock::named("perf.sampler", Vec::new()),
                period: AtomicU64::new(0),
                last: AtomicU64::new(0),
                deadline: AtomicU64::new(0),
                window: AtomicU64::new(0),
                busy: AtomicU64::new(0),
                fires: AtomicU64::new(0),
                fire_ns: AtomicU64::new(0),
            })
            .collect()
    })
}

/// The CPUs whose sampler may serve `ev`.
fn cpus_of(ev: &Event) -> impl Iterator<Item = usize> {
    let n = samplers().len();
    let one = ev.cpu();
    (0..n).filter(move |c| one.is_none_or(|o| o == *c))
}

/// The sampler lists gain `ev` (a sampling event); its timers start when it is enabled.
pub(crate) fn attach(ev: &Arc<Event>) {
    if !ev.sampling() {
        return;
    }
    let _g = CHANGE.lock();
    for c in cpus_of(ev) {
        let s = &samplers()[c];
        loop {
            let n = s.events.lock().len();
            let mut v: Vec<Arc<Event>> = Vec::new();
            if v.try_reserve_exact(n + 1).is_err() {
                klog::event!(Warn, "perf", "sampler.no_memory", cpu = c);
                break;
            }
            let old = {
                let mut l = s.events.lock();
                if l.len() != n {
                    None
                } else {
                    v.extend(l.iter().cloned());
                    v.push(ev.clone());
                    Some(core::mem::replace(&mut *l, v))
                }
            };
            if old.is_some() {
                break;
            }
        }
    }
    drop(_g);
    refresh(ev);
}

/// The sampler lists lose `ev` (its close); the references go outside the IRQ-safe lock.
pub(crate) fn detach(ev: &Arc<Event>) {
    if !ev.sampling() {
        return;
    }
    let g = CHANGE.lock();
    let mut gone: Vec<Vec<Arc<Event>>> = Vec::new();
    for c in cpus_of(ev) {
        let s = &samplers()[c];
        let n = s.events.lock().len();
        let mut v: Vec<Arc<Event>> = Vec::new();
        if v.try_reserve_exact(n).is_err() {
            continue;
        }
        let old = {
            let mut l = s.events.lock();
            v.extend(l.iter().filter(|e| !Arc::ptr_eq(e, ev)).cloned());
            core::mem::replace(&mut *l, v)
        };
        let _ = gone.try_reserve(1);
        gone.push(old);
    }
    drop(g);
    drop(gone);
    refresh(ev);
}

/// The period of every sampler `ev` touches is worked out again; a sampler with work starts, one without stops.
pub(crate) fn refresh(ev: &Event) {
    if ev.period.load(Ordering::Relaxed) == 0 || SAMPLERS.get().is_none() {
        return;
    }
    let _g = CHANGE.lock();
    for c in cpus_of(ev) {
        refresh_cpu(c);
    }
}

fn refresh_cpu(c: usize) {
    let s = &samplers()[c];
    let want = {
        let l = s.events.lock();
        l.iter()
            .filter(|e| !e.closed.load(Ordering::Relaxed) && e.st.lock().enabled)
            .map(|e| {
                let p = e.period.load(Ordering::Relaxed).max(min_period());
                if e.spread.load(Ordering::Relaxed) { p.max(min_period().saturating_mul(hal::percpu::cpu_count().max(1) as u64)) } else { p }
            })
            .min()
            .unwrap_or(0)
    };
    let was = s.period.swap(want, Ordering::AcqRel);
    if want == 0 {
        s.timer.cancel();
        return;
    }
    if was != 0 && s.timer.is_armed() {
        // Running: the next expiry takes the new period.
        return;
    }
    if !hal::smp::is_online(c) && c != hal::percpu::this_cpu() {
        return;
    }
    for _ in 0..50 {
        match hal::smp::call_on(c, start_here, c as u64, time::now_ns, 20_000_000) {
            Ok(_) => return,
            Err(hal::smp::CallError::Busy) => sched::yield_now(),
            Err(e) => {
                klog::event!(Warn, "perf", "sampler.start_failed", cpu = c, why = alloc::format!("{e:?}").as_str());
                return;
            }
        }
    }
}

/// The shortest timer period: Linux's 10 µs, or longer while `perf_event_max_sample_rate` is lower.
fn min_period() -> u64 {
    let rate = super::MAX_SAMPLE_RATE.load(Ordering::Relaxed).max(1);
    MIN_PERIOD_NS.max(1_000_000_000 / rate)
}

/// On CPU `c` (its cross-CPU call, interrupts off): the first expiry one period from now.
fn start_here(c: u64) -> u64 {
    let Some(s) = SAMPLERS.get().and_then(|v| v.get(c as usize)) else { return 0 };
    let p = s.period.load(Ordering::Acquire);
    if p == 0 {
        return 0;
    }
    let now = time::now_ns();
    s.last.store(now, Ordering::Relaxed);
    s.deadline.store(now + p, Ordering::Relaxed);
    s.timer.arm(now + p);
    1
}

/// Who runs on this CPU now, as a sample names it.
struct Who {
    pid: u32,
    tid: u32,
    idle: bool,
    /// The scheduler id (thread events match on it).
    kid: u64,
    /// A program thread: its registers when it last left user mode.
    user: Option<(u64, u64, u64)>,
}

fn who() -> Who {
    let Some(t) = sched::current_thread() else { return Who { pid: 0, tid: 0, idle: true, kid: 0, user: None } };
    if t.is_idle_thread() {
        return Who { pid: 0, tid: 0, idle: true, kid: t.id, user: None };
    }
    match t.user_ids() {
        Some((pid, tid)) => Who { pid, tid, idle: false, kid: t.id, user: Some(t.user_regs()) },
        None => Who { pid: 0, tid: KTHREAD_TID | (t.id as u32 & !KTHREAD_TID), idle: false, kid: t.id, user: None },
    }
}

/// The timer of CPU `c` expired (hard-IRQ context, on `c`).
fn fire(c: usize) {
    let t0 = time::now_ns();
    let Some(s) = SAMPLERS.get().and_then(|v| v.get(c)) else { return };
    let p = s.period.load(Ordering::Acquire);
    if p == 0 || hal::percpu::this_cpu() != c {
        return;
    }
    let elapsed = t0.saturating_sub(s.last.swap(t0, Ordering::Relaxed));
    let regs = hal::sampling::interrupted();
    let me = who();
    for ev in s.events.lock().iter() {
        tick(ev, c, elapsed, t0, &me, regs.as_ref());
    }
    let end = time::now_ns();
    let took = end.saturating_sub(t0);
    // `hrtimer_forward_now` from the end of this expiry (not its start): a pass longer than the period must not
    // arm a deadline that is already over.
    let p = p.max(min_period());
    let mut next = s.deadline.load(Ordering::Relaxed).saturating_add(p);
    if next <= end {
        next = end + p;
    }
    if let Some(w) = over_budget(s, end, took) {
        next = next.max(w);
        THROTTLED.inc();
    }
    s.deadline.store(next, Ordering::Relaxed);
    s.timer.arm(next);
    TICKS.inc();
    TICK_NS.add(took);
    s.fires.fetch_add(1, Ordering::Relaxed);
    s.fire_ns.fetch_add(took, Ordering::Relaxed);
}

/// `perf_cpu_time_max_percent` for this CPU: the interrupt time of the current 1 ms window; past the share, the
/// start of the next window (the timer waits for it).
fn over_budget(s: &Sampler, end: u64, took: u64) -> Option<u64> {
    if super::throttling_off() {
        return None;
    }
    let mut w = s.window.load(Ordering::Relaxed);
    if end.saturating_sub(w) >= WINDOW_NS {
        w = end;
        s.window.store(w, Ordering::Relaxed);
        s.busy.store(0, Ordering::Relaxed);
    }
    let busy = s.busy.fetch_add(took, Ordering::Relaxed) + took;
    let budget = WINDOW_NS / 100 * super::CPU_TIME_MAX_PERCENT.load(Ordering::Relaxed);
    (busy >= budget).then_some(w + WINDOW_NS)
}

fn tick(ev: &Event, cpu: usize, elapsed: u64, now: u64, me: &Who, regs: Option<&Interrupted>) {
    match &ev.target {
        Target::Cpu(c) if *c != cpu => return,
        Target::Thread { cpu: Some(c), .. } if *c != cpu => return,
        Target::Thread { .. } if ev.sched_id() == 0 || ev.sched_id() != me.kid => return,
        _ => {}
    }
    let period = ev.period.load(Ordering::Relaxed);
    let hit_limit;
    {
        let mut st = ev.st.lock();
        if !st.enabled || period == 0 {
            return;
        }
        st.acc = st.acc.saturating_add(elapsed);
        if st.acc < period {
            return;
        }
        // One sample per full period; a backlog of more than one (a long stretch with interrupts off) is dropped.
        st.acc = if st.acc >= 2 * period { 0 } else { st.acc - period };
        let Some(r) = regs else { return };
        let a = &ev.attr;
        if (a.has(flag::EXCLUDE_USER) && r.user) || (a.has(flag::EXCLUDE_KERNEL) && !r.user) || (a.has(flag::EXCLUDE_IDLE) && me.idle) {
            SAMPLES_SKIPPED.inc();
            return;
        }
        hit_limit = limit_hit(&mut st);
    }
    let Some(r) = regs else { return };
    let mut chain = [0u64; CHAIN_BUF];
    let n = if ev.attr.sample_type & SAMPLE_CALLCHAIN != 0 { callchain(ev, r, me, &mut chain) } else { 0 };
    let ident = Ident { pid: me.pid, tid: me.tid, time: now.wrapping_add(ev.clock_off), id: ev.id, stream_id: ev.id, cpu: cpu as u32 };
    let mut sib = [perf_abi::record::ReadValue::default(); MAX_GROUP_READ];
    let (read, nsib) = if ev.attr.sample_type & SAMPLE_READ == 0 {
        (Default::default(), 0)
    } else if ev.attr.read_format & FORMAT_GROUP != 0 {
        // `perf_output_read_group` from the leader (a member samples with its leader's values first).
        let l = match &ev.leader {
            Some(l) if !l.closed.load(Ordering::Relaxed) => &**l,
            _ => ev,
        };
        (l.read_value(), l.sibling_values(&mut sib))
    } else {
        (ev.read_value(), 0)
    };
    let smp = Sample { misc: if r.user { MISC_USER } else { MISC_KERNEL }, ip: r.rip, ident, addr: 0, period, read, siblings: &sib[..nsib], callchain: &chain[..n] };
    let mut buf = [0u8; REC_BUF];
    if let Some(rec) = record::sample(&mut buf, ev.attr.sample_type, ev.attr.read_format, &smp)
        && ev.output(rec, &ident, true)
    {
        SAMPLES.inc();
        ev.samples.fetch_add(1, Ordering::Relaxed);
    }
    if hit_limit {
        ev.stop_in_irq();
        ev.wq.wake_poll(vfs::poll::Events::HUP);
    }
}

/// `get_perf_callchain`: the kernel part (the interrupted ip, then the frame-pointer chain), then the program's
/// (its ip, then its frame pointers through present pages), each after its context mark; at most `max_stack`
/// addresses and `perf_event_max_contexts_per_stack` marks.
fn callchain(ev: &Event, r: &Interrupted, me: &Who, out: &mut [u64; CHAIN_BUF]) -> usize {
    let max = (ev.max_stack as usize).min(CHAIN_MAX);
    let max_ctx = super::MAX_CONTEXTS.load(Ordering::Relaxed) as usize;
    let mut c = Chain { out, len: 0, nr: 0, contexts: 0, max, max_ctx, maxed: false };
    if !ev.attr.has(flag::EXCLUDE_CALLCHAIN_KERNEL) && !r.user {
        c.mark(CONTEXT_KERNEL);
        if c.store(r.rip) {
            let room = c.max.saturating_sub(c.nr);
            hal::sampling::interrupted_kernel_chain(room, &mut |a| {
                c.store(a);
            });
        }
    }
    if !ev.attr.has(flag::EXCLUDE_CALLCHAIN_USER) {
        // A thread that never ran user code yet has no user registers (rip 0): no user part, as Linux for a task
        // without a user context.
        let regs = if r.user { Some((r.rip, r.rbp, r.rsp)) } else { me.user.filter(|u| u.0 != 0) };
        if let Some((ip, mut fp, _sp)) = regs {
            c.mark(CONTEXT_USER);
            if c.store(ip) {
                let root = hal::paging::current_root();
                while c.nr < c.max {
                    if fp == 0 || fp % 8 != 0 || fp.checked_add(16).is_none_or(|e| e > hal::USER_TOP) {
                        break;
                    }
                    let mut f = [0u8; 16];
                    if hal::usercopy::copy_from_user_nofault(root, &mut f, fp) != 16 {
                        break;
                    }
                    let next = u64::from_le_bytes(f[..8].try_into().unwrap_or([0; 8]));
                    let ret = u64::from_le_bytes(f[8..].try_into().unwrap_or([0; 8]));
                    if !c.store(ret) || next <= fp {
                        break;
                    }
                    fp = next;
                }
            }
        }
    }
    c.len
}

struct Chain<'a> {
    out: &'a mut [u64; CHAIN_BUF],
    len: usize,
    /// Addresses stored (marks not counted, `ctx->nr`).
    nr: usize,
    contexts: usize,
    max: usize,
    max_ctx: usize,
    maxed: bool,
}

impl Chain<'_> {
    /// `perf_callchain_store_context`.
    fn mark(&mut self, m: u64) {
        if self.contexts < self.max_ctx && self.len < CHAIN_BUF {
            self.out[self.len] = m;
            self.len += 1;
            self.contexts += 1;
        } else {
            self.maxed = true;
        }
    }
    /// `perf_callchain_store`: false once full.
    fn store(&mut self, ip: u64) -> bool {
        if self.nr < self.max && !self.maxed && self.len < CHAIN_BUF {
            self.out[self.len] = ip;
            self.len += 1;
            self.nr += 1;
            true
        } else {
            false
        }
    }
}

/// Runs `f` with the samplers' total expiries and time (ns) so far (ktests, the overhead numbers).
pub(crate) fn cost() -> (u64, u64) {
    (TICKS.get(), TICK_NS.get())
}

/// CPU `c`'s sampler expiries and their time (ns) so far (ktests).
pub(crate) fn cpu_cost(c: usize) -> (u64, u64) {
    SAMPLERS.get().and_then(|v| v.get(c)).map_or((0, 0), |s| (s.fires.load(Ordering::Relaxed), s.fire_ns.load(Ordering::Relaxed)))
}

/// The timer period an unprivileged thread event gets at least (ktests).
pub(crate) fn spread_period() -> u64 {
    min_period().saturating_mul(hal::percpu::cpu_count().max(1) as u64)
}

/// Whether CPU `c`'s sampler is running (ktests).
pub(crate) fn running(c: usize) -> bool {
    SAMPLERS.get().and_then(|v| v.get(c)).is_some_and(|s| s.period.load(Ordering::Relaxed) != 0)
}
