//! `HrTimer` (contract `C: HrTimer`): one-shot high-resolution timers in **per-CPU queues**, each programmed into
//! its CPU's LAPIC timer at the earliest deadline (CP-5; spec/core.md §7.2; Linux `kernel/time/hrtimer.c`).
//!
//! - `arm` puts the timer into the queue of the CPU it runs on (so sched's per-CPU tick, armed by its own CPU,
//!   fires there and the interrupt exit can preempt), or keeps it on the CPU that is running its callback right
//!   now (that CPU reprograms when the callback returns, as Linux keeps a running timer on its base). A timer is
//!   in at most one queue; re-arming moves it. A deadline in the past fires at once. A re-arm while the poller
//!   runs the callback goes to a live CPU's queue; a queue run never starts a callback whose run is still in
//!   progress (the `running_on` claim): it looks again `RETRY_NS` later.
//! - The LAPIC timer interrupt (`lapic::on_timer`) pops every due timer and calls it in hard-IRQ context (with
//!   the queue lock released, so a callback may re-arm or cancel itself or others), then programs the next
//!   deadline. A callback that keeps re-arming in the past cannot hold the CPU: one interrupt runs a bounded
//!   number of callbacks and comes back 2 µs later for the rest.
//! - Arming and cancelling never allocate (callbacks re-arm from hard-IRQ context): every queue keeps room for
//!   every timer that exists, made when a timer is created (`new`, thread context). Room is made outside the
//!   queue locks (allocation may wait for other CPUs, which may be spinning on a queue lock). Should that
//!   allocation fail (memory exhausted), `new` logs `hrtimer.no_room` and an arm that finds no room in its
//!   CPU's queue goes to the polled queue instead (`interrupt_driven()` is false while it holds timers, so the
//!   scheduler keeps polling); only when that is full too is the arm lost, counted and logged by the poller.
//! - Dropping the last clone marks the timer dead first: no arm (a late re-arm racing the drop, the adoption of
//!   the polled queue) can queue it again, and the drop cancels until no queue and no other CPU holds it.
//! - Until a CPU's LAPIC timer runs (early boot, CONFIG `hrtimer=poll`, a CPU whose timer failed its
//!   calibration), timers armed there go to one **polled** queue that the `time.timers` poller fires (thread
//!   context, as in G0). `interrupt_driven()` says when nothing depends on polling any more.
//!
//! Locks: one `IrqSpinLock` per queue (`time.queue`, `time.polled`), never two at once; callbacks run with none.
//! Callbacks must be short and IRQ-safe: no heap, no klog, no sleeping; `klog::counter!`s, `IrqSpinLock`s and
//! wake-ups are fine.

use crate::heap::{Heap, Keyed};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use hal::percpu::PerCpu;
use ksync::IrqSpinLock;

/// Queue index of a timer in no queue.
const NONE: usize = usize::MAX;
/// Queue index of the polled queue (also the "CPU" running a polled callback).
const POLLED_Q: usize = usize::MAX - 1;
/// Callbacks one interrupt may run before it hands the rest to a fresh interrupt.
const BUDGET_EXTRA: usize = 16;
/// How soon the rest comes back after the budget ran out.
pub const RETRY_NS: u64 = 2_000;
/// No deadline programmed.
pub(crate) const NO_DEADLINE: u64 = u64::MAX;

klog::counter!(pub ARMED, "time.hrtimer_armed");
klog::counter!(pub FIRED, "time.hrtimer_fired");
klog::counter!(pub FIRED_POLLED, "time.hrtimer_fired_polled");
klog::counter!(pub NO_ROOM, "time.hrtimer_no_room");
klog::counter!(pub BUDGET_HIT, "time.hrtimer_budget_hit");
klog::counter!(pub DROP_WAIT_GAVE_UP, "time.hrtimer_drop_wait_gave_up");
klog::counter!(pub ROOM_FALLBACK, "time.hrtimer_room_fallback");
klog::counter!(pub ARM_DEAD, "time.hrtimer_arm_dead");

/// Set where an arm went to the polled queue for want of room (`ROOM_FALLBACK`) or was lost (`NO_ROOM`); the
/// poller (thread context) logs it, never the arm itself (hard-IRQ context).
static FELL_BACK: AtomicBool = AtomicBool::new(false);
static LOST: AtomicBool = AtomicBool::new(false);
/// Timers in the polled queue (kept under its lock; read lock-free by `interrupt_driven`).
static POLLED_LEN: AtomicUsize = AtomicUsize::new(0);
/// How long a drop waits for a callback run on another CPU (TSC ticks when the rate is unknown).
const DROP_WAIT_NS: u64 = 100_000_000;
const DROP_WAIT_TICKS_UNKNOWN: u64 = 1_000_000_000;

pub(crate) struct TimerInner {
    func: fn(usize),
    arg: usize,
    deadline: AtomicU64,
    seq: AtomicU64,
    /// The queue holding it (`NONE`, a CPU, `POLLED_Q`); changed only under that queue's lock.
    queue: AtomicUsize,
    /// Its place in that queue's heap (under the same lock).
    pos: AtomicUsize,
    /// The CPU (or `POLLED_Q`) running its callback now, `NONE` otherwise; changed under that queue's lock.
    running_on: AtomicUsize,
    /// Its last handle is being dropped: arms are refused (checked under the target queue's lock).
    dead: AtomicBool,
}

impl Keyed for Arc<TimerInner> {
    fn key(&self) -> (u64, u64) {
        (self.deadline.load(Ordering::Relaxed), self.seq.load(Ordering::Relaxed))
    }
    fn pos(&self) -> usize {
        self.pos.load(Ordering::Relaxed)
    }
    fn set_pos(&self, p: usize) {
        self.pos.store(p, Ordering::Relaxed)
    }
}

pub(crate) struct Queue {
    heap: Heap<Arc<TimerInner>>,
    /// The deadline programmed into this CPU's LAPIC timer (`NO_DEADLINE`: none).
    pub(crate) programmed: u64,
}

impl Queue {
    const fn new() -> Queue {
        Queue { heap: Heap::new(), programmed: NO_DEADLINE }
    }
    pub(crate) fn earliest(&self) -> Option<u64> {
        self.heap.peek().map(|t| t.deadline.load(Ordering::Relaxed))
    }
    pub(crate) fn len(&self) -> usize {
        self.heap.len()
    }
}

static POLLED: IrqSpinLock<Queue> = IrqSpinLock::named("time.polled", Queue::new());
static QUEUES: PerCpu<IrqSpinLock<Queue>> = PerCpu::new(|_| IrqSpinLock::named("time.queue", Queue::new()));
/// Timers that exist (every queue has room for this many).
static TOTAL: AtomicUsize = AtomicUsize::new(0);
static SEQ: AtomicU64 = AtomicU64::new(0);

fn queue(idx: usize) -> Option<&'static IrqSpinLock<Queue>> {
    if idx == POLLED_Q { Some(&POLLED) } else { QUEUES.find_for(idx) }
}

/// After a change to queue `idx` under its lock: keeps `POLLED_LEN` current.
fn note_len(idx: usize, q: &Queue) {
    if idx == POLLED_Q {
        POLLED_LEN.store(q.len(), Ordering::Release);
    }
}

/// Timers waiting in the polled queue (0 when every timer fires by interrupt).
pub fn polled_len() -> usize {
    POLLED_LEN.load(Ordering::Acquire)
}

/// Every possible CPU's queue exists (thread context; allocates). The BSP calls it before any AP starts.
pub(crate) fn prepare_queues() {
    QUEUES.cover_possible();
}

/// Room for `n` timers in `q`, made outside its lock; the old (empty) storage is freed outside it too. False
/// when the memory for it could not be had.
fn make_room(q: &IrqSpinLock<Queue>, n: usize) -> bool {
    loop {
        let have = q.lock().heap.capacity();
        if have >= n {
            return true;
        }
        let mut spare: Vec<Arc<TimerInner>> = Vec::new();
        // Doubling first; if that much is not to be had, exactly enough.
        if spare.try_reserve(n.max(have * 2).max(4)).is_err() && spare.try_reserve(n).is_err() {
            return false;
        }
        let old = {
            let mut g = q.lock();
            if g.heap.capacity() >= n {
                None
            } else {
                Some(g.heap.replace_storage(spare))
            }
        };
        drop(old);
    }
}

/// Room for `n` timers in every queue, the polled one first (it is the fallback); false if any is short.
fn ensure_room(n: usize) -> bool {
    prepare_queues();
    let mut ok = make_room(&POLLED, n);
    for cpu in 0..hal::percpu::possible_cpus().max(hal::percpu::cpu_count()) {
        if let Some(q) = QUEUES.find_for(cpu) {
            ok &= make_room(q, n);
        }
    }
    ok
}

/// A one-shot high-resolution timer. Callbacks run in hard-IRQ context (or, while polled, in the poller's
/// thread): short, no sleeping, no heap. Clones share one timer; it is cancelled when the last clone is dropped
/// (drop it in thread context, and never from its own callback).
#[derive(Clone)]
pub struct HrTimer {
    h: Arc<Handle>,
}

struct Handle {
    t: Arc<TimerInner>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        // Dead first: from here on no arm queues it again (an arm that already passed its check under a queue
        // lock is undone by the cancel below, which takes that lock after it).
        self.t.dead.store(true, Ordering::SeqCst);
        // Cancel until nothing but this handle holds the timer: a queue, a callback run on another CPU, or the
        // adoption of the polled queue (which holds it between two queues). Then the memory is freed here and
        // not in an interrupt. Bounded (B4): one callback run is short; never waits for a run on this very CPU
        // (a callback dropping its own timer).
        let me = hal::percpu::this_cpu();
        let hz = crate::tsc_hz();
        let limit = if hz == 0 { DROP_WAIT_TICKS_UNKNOWN } else { crate::clock::scale(DROP_WAIT_NS, crate::clock::tick_mult(hz)) };
        let start = hal::cpu::rdtsc();
        loop {
            cancel_inner(&self.t);
            if Arc::strong_count(&self.t) <= 1 || self.t.running_on.load(Ordering::Acquire) == me {
                break;
            }
            if hal::cpu::rdtsc().wrapping_sub(start) > limit {
                // A callback still runs elsewhere: it frees the memory when it returns; dead, it cannot re-arm,
                // so the timer holds no queue slot and `TOTAL` may go down.
                DROP_WAIT_GAVE_UP.inc();
                cancel_inner(&self.t);
                // Drops happen in thread context (see `HrTimer`); klog is IRQ-safe should one not.
                klog::event!(Warn, "time", "hrtimer.drop_wait_gave_up",
                    running_on = self.t.running_on.load(Ordering::Acquire), wait_ms = DROP_WAIT_NS / 1_000_000;
                    "a dropped timer was still held elsewhere after the wait (a callback run on another CPU, or the polled queue's adoption): dead, it cannot be queued again, and it is freed when that holder lets go");
                break;
            }
            hal::cpu::spin_hint();
        }
        TOTAL.fetch_sub(1, Ordering::AcqRel);
    }
}

impl HrTimer {
    /// A timer calling `func(arg)` when it fires (thread context: makes room in every queue).
    pub fn new(func: fn(usize), arg: usize) -> Self {
        let t = Arc::new(TimerInner {
            func,
            arg,
            deadline: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            queue: AtomicUsize::new(NONE),
            pos: AtomicUsize::new(0),
            running_on: AtomicUsize::new(NONE),
            dead: AtomicBool::new(false),
        });
        let n = TOTAL.fetch_add(1, Ordering::AcqRel) + 1;
        if !ensure_room(n) {
            NO_ROOM.inc();
            klog::event!(Error, "time", "hrtimer.no_room", timers = n;
                "no memory for room in every timer queue: arms that find none go to the polled queue (slower), or are lost when it is full too");
        }
        HrTimer { h: Arc::new(Handle { t }) }
    }
    /// Fires once at `deadline_ns` (monotonic, `now_ns`); re-arming moves the deadline; a past deadline fires
    /// at once. IRQ-safe, never allocates.
    pub fn arm(&self, deadline_ns: u64) {
        arm_inner(&self.h.t, deadline_ns);
    }
    pub fn arm_in(&self, ns: u64) {
        self.arm(crate::now_ns().saturating_add(ns))
    }
    /// Takes the timer out of its queue; true if it was armed. A callback already running is not waited for.
    pub fn cancel(&self) -> bool {
        cancel_inner(&self.h.t)
    }
    pub fn is_armed(&self) -> bool {
        self.h.t.queue.load(Ordering::Acquire) != NONE
    }
    /// The armed deadline (`None` when not armed).
    pub fn deadline(&self) -> Option<u64> {
        self.is_armed().then(|| self.h.t.deadline.load(Ordering::Acquire))
    }
    /// The CPU whose LAPIC will fire it (`None`: not armed, or in the polled queue).
    pub fn cpu(&self) -> Option<usize> {
        let q = self.h.t.queue.load(Ordering::Acquire);
        (q != NONE && q != POLLED_Q).then_some(q)
    }
    /// Whether its callback runs right now (on some CPU).
    pub fn is_running(&self) -> bool {
        self.h.t.running_on.load(Ordering::Acquire) != NONE
    }
}

fn arm_inner(t: &Arc<TimerInner>, deadline: u64) {
    let _irq = hal::cpu::IrqGuard::new();
    let me = hal::percpu::this_cpu();
    ARMED.inc();
    // Set when this CPU's queue had no room: the timer goes to the polled queue instead.
    let mut fallback = false;
    loop {
        if t.dead.load(Ordering::SeqCst) {
            ARM_DEAD.inc();
            return;
        }
        let running = t.running_on.load(Ordering::Acquire);
        let target = if fallback {
            POLLED_Q
        } else if running != NONE && running != POLLED_Q {
            // That CPU reprograms its timer when the callback returns.
            running
        } else if crate::lapic::live(me) {
            me
        } else {
            POLLED_Q
        };
        let cur = t.queue.load(Ordering::Acquire);
        if cur != NONE && cur != target {
            if let Some(q) = queue(cur) {
                let mut g = q.lock();
                if t.queue.load(Ordering::Acquire) == cur {
                    g.heap.remove(t.pos());
                    t.queue.store(NONE, Ordering::Release);
                    note_len(cur, &g);
                }
            }
            continue;
        }
        let Some(q) = queue(target) else {
            // A CPU without a queue cannot be live; only an unprepared boot gets here.
            if target == POLLED_Q {
                return lost();
            }
            fallback = true;
            continue;
        };
        let mut g = q.lock();
        // Re-check under the lock: the run may have ended, or another CPU moved the timer meanwhile.
        let held = t.queue.load(Ordering::Acquire);
        if t.running_on.load(Ordering::Acquire) != running || !(held == NONE || held == target) {
            continue;
        }
        // Under the target's lock: a drop that marked it dead before cancels after this lock is released.
        if t.dead.load(Ordering::SeqCst) {
            ARM_DEAD.inc();
            return;
        }
        if held == target {
            // In this heap: its key changes under this heap's lock only.
            t.deadline.store(deadline, Ordering::Relaxed);
            t.seq.store(SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
            g.heap.fix(t.pos());
        } else {
            // In no heap: claim it for this queue first. Two CPUs arming the same timer at once both saw NONE,
            // each under its own queue's lock; without the claim both pushed it, it sat in two heaps with one
            // position, and a later fix/remove indexed past a heap's end (INT-A2a: `heap.rs:98` panics at -smp 16,
            // MM-5b's and AB-5's reports). A timer goes from NONE to a queue only through this claim (or the
            // retry claim in `run_queue`), and back to NONE only under the lock of the queue it is in.
            if t.queue.compare_exchange(NONE, target, Ordering::AcqRel, Ordering::Acquire).is_err() {
                continue;
            }
            t.deadline.store(deadline, Ordering::Relaxed);
            t.seq.store(SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
            if g.heap.push_within(t.clone()).is_ok() {
                note_len(target, &g);
            } else {
                t.queue.store(NONE, Ordering::Release);
                drop(g);
                if target != POLLED_Q {
                    // Broken room invariant (`new` could not get the memory): never allocate here, poll it.
                    fallback = true;
                    ROOM_FALLBACK.inc();
                    FELL_BACK.store(true, Ordering::Release);
                    continue;
                }
                return lost();
            }
        }
        if target == me {
            crate::lapic::program(&mut g, me);
        }
        return;
    }
}

/// An arm with room nowhere: counted and flagged for the poller's event (never logged from here).
fn lost() {
    NO_ROOM.inc();
    LOST.store(true, Ordering::Release);
}

fn cancel_inner(t: &Arc<TimerInner>) -> bool {
    loop {
        let cur = t.queue.load(Ordering::Acquire);
        if cur == NONE {
            return false;
        }
        let Some(q) = queue(cur) else { return false };
        let mut g = q.lock();
        if t.queue.load(Ordering::Acquire) == cur {
            g.heap.remove(t.pos());
            t.queue.store(NONE, Ordering::Release);
            note_len(cur, &g);
            return true;
        }
    }
}

/// Runs the due timers of queue `idx` (a CPU's from its LAPIC interrupt, or the polled one) and, for a CPU
/// queue, programs its next deadline. Returns how many fired.
fn run_queue(idx: usize) -> usize {
    let Some(q) = queue(idx) else { return 0 };
    let mut fired = 0;
    let mut g = q.lock();
    if idx != POLLED_Q {
        // The one-shot that brought us here is spent.
        g.programmed = NO_DEADLINE;
    }
    let mut budget = g.len() + BUDGET_EXTRA;
    loop {
        let now = crate::now_ns();
        let due = g.earliest().is_some_and(|d| d <= now);
        if !due {
            break;
        }
        if budget == 0 {
            BUDGET_HIT.inc();
            if idx != POLLED_Q {
                crate::lapic::program_at(&mut g, idx, now.saturating_add(RETRY_NS));
            }
            return fired;
        }
        budget -= 1;
        let Some(t) = g.heap.pop() else { break };
        t.queue.store(NONE, Ordering::Release);
        // A callback never runs twice at once: a run still in progress elsewhere (the poller's, re-armed from
        // a live CPU, or this CPU's own poller thread interrupted inside it) keeps the claim, and this queue
        // looks again `RETRY_NS` later (review INT-A1d core 3; Linux's hrtimer never overlaps a callback).
        if t.running_on.compare_exchange(NONE, idx, Ordering::AcqRel, Ordering::Acquire).is_err() {
            let mut gone = None;
            if t.dead.load(Ordering::SeqCst) {
                // Dropped meanwhile: not queued again (its drop cancels what it finds).
                gone = Some(t);
            } else if t.queue.compare_exchange(NONE, idx, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                // Claimed for this queue (an arm on another CPU may have claimed it since the pop: then it is
                // queued there with that arm's deadline and nothing is left to do here).
                t.deadline.store(now.saturating_add(RETRY_NS), Ordering::Relaxed);
                t.seq.store(SEQ.fetch_add(1, Ordering::Relaxed), Ordering::Relaxed);
                // Room exists: the timer was popped from this heap just now.
                if let Err(t) = g.heap.push_within(t) {
                    t.queue.store(NONE, Ordering::Release);
                    lost();
                    gone = Some(t);
                }
            } else {
                gone = Some(t);
            }
            note_len(idx, &g);
            if gone.is_some() {
                // Possibly the last reference: freed outside the lock.
                drop(g);
                drop(gone);
                g = q.lock();
            }
            continue;
        }
        note_len(idx, &g);
        drop(g);
        (t.func)(t.arg);
        fired += 1;
        g = q.lock();
        // Only this run's claim: a run elsewhere is never marked as ended by this one.
        let _ = t.running_on.compare_exchange(idx, NONE, Ordering::AcqRel, Ordering::Acquire);
        // The last reference may be this one (its owner dropped it meanwhile and waits): free outside the lock.
        drop(g);
        drop(t);
        g = q.lock();
    }
    if idx != POLLED_Q {
        crate::lapic::program(&mut g, idx);
    }
    drop(g);
    if idx == POLLED_Q {
        FIRED_POLLED.add(fired as u64);
    } else {
        FIRED.add(fired as u64);
    }
    fired
}

/// The LAPIC timer interrupt's work on this CPU.
pub(crate) fn run_cpu(cpu: usize) -> usize {
    run_queue(cpu)
}

/// Fires the due timers of the polled queue (the `time.timers` poller; thread context). Timers on a CPU whose
/// LAPIC timer runs fire by interrupt and are not touched here.
pub fn poll_timers() -> usize {
    if FELL_BACK.swap(false, Ordering::AcqRel) {
        klog::event!(Warn, "time", "hrtimer.room_fallback", count = ROOM_FALLBACK.get(), timers = count();
            "a CPU's timer queue had no room: timers were moved to the polled queue");
    }
    if LOST.swap(false, Ordering::AcqRel) {
        klog::event!(Error, "time", "hrtimer.lost", count = NO_ROOM.get(), timers = count();
            "a timer could not be queued anywhere (no memory for queue room): its sleeper may wait forever");
    }
    run_queue(POLLED_Q)
}

/// Moves every timer of the polled queue into this CPU's queue (its LAPIC timer just went live). IRQ-safe.
pub(crate) fn adopt_polled() -> usize {
    let mut moved = 0;
    // Bounded by what the queue held at the start: an arm without room here falls back to the polled queue
    // again (`ROOM_FALLBACK`), and popping it once more would never end.
    let held = POLLED.lock().len();
    for _ in 0..held {
        let t = {
            let mut g = POLLED.lock();
            let Some(t) = g.heap.pop() else { break };
            t.queue.store(NONE, Ordering::Release);
            note_len(POLLED_Q, &g);
            t
        };
        arm_inner(&t, t.deadline.load(Ordering::Relaxed));
        moved += 1;
    }
    moved
}

/// Earliest armed deadline on any queue (tickless decisions, tests).
pub fn next_deadline() -> Option<u64> {
    let mut best = POLLED.lock().earliest();
    for cpu in 0..hal::percpu::cpu_count() {
        if let Some(q) = QUEUES.find_for(cpu) {
            let d = q.lock().earliest();
            best = match (best, d) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
        }
    }
    best
}

/// Armed timers in `cpu`'s queue (`None`: the polled queue) and its programmed deadline.
pub fn queue_state(cpu: Option<usize>) -> Option<(usize, u64)> {
    let q = match cpu {
        Some(c) => QUEUES.find_for(c)?,
        None => &POLLED,
    };
    let g = q.lock();
    Some((g.len(), g.programmed))
}

/// Timers that exist now.
pub fn count() -> usize {
    TOTAL.load(Ordering::Acquire)
}

/// Runs `f` with `cpu`'s queue locked (the LAPIC module programs from there).
pub(crate) fn with_cpu_queue<R>(cpu: usize, f: impl FnOnce(&mut Queue) -> R) -> Option<R> {
    let q = QUEUES.find_for(cpu)?;
    let mut g = q.lock();
    Some(f(&mut g))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU32;
    use std::sync::Mutex;
    extern crate std;

    // The host build has one CPU and no LAPIC: every timer goes to the polled queue.
    static SERIAL: Mutex<()> = Mutex::new(());
    static ORDER: Mutex<Vec<usize>> = Mutex::new(Vec::new());
    static FIRED_N: AtomicU32 = AtomicU32::new(0);

    fn rec(arg: usize) {
        ORDER.lock().unwrap().push(arg);
    }

    fn setup() -> std::sync::MutexGuard<'static, ()> {
        let g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        crate::CLOCK.set_rate(hal::cpu::rdtsc(), 1_000_000_000);
        ORDER.lock().unwrap().clear();
        g
    }

    #[test]
    fn fire_in_deadline_order_ties_in_arm_order() {
        let _g = setup();
        let ts: Vec<HrTimer> = (0..8).map(|i| HrTimer::new(rec, i)).collect();
        for (i, d) in [50u64, 10, 30, 10, 0, 70, 30, 20].iter().enumerate() {
            ts[i].arm(*d);
        }
        assert!(ts.iter().all(|t| t.is_armed() && t.cpu().is_none()), "polled on the host");
        assert_eq!(poll_timers(), 8);
        assert_eq!(*ORDER.lock().unwrap(), alloc::vec![4, 1, 3, 7, 2, 6, 0, 5]);
        assert!(ts.iter().all(|t| !t.is_armed()));
    }

    #[test]
    fn cancel_and_rearm() {
        let _g = setup();
        let a = HrTimer::new(rec, 1);
        let b = HrTimer::new(rec, 2);
        a.arm(0);
        b.arm(u64::MAX);
        assert!(b.cancel());
        assert!(!b.cancel(), "second cancel finds nothing");
        a.arm(u64::MAX - 1); // moved to the far future
        assert_eq!(poll_timers(), 0);
        assert_eq!(a.deadline(), Some(u64::MAX - 1));
        a.arm(0); // and back
        assert_eq!(poll_timers(), 1);
        assert_eq!(*ORDER.lock().unwrap(), alloc::vec![1]);
        assert_eq!(next_deadline(), None);
    }

    static SELF_REARM: Mutex<Option<HrTimer>> = Mutex::new(None);
    fn rearm_self(_: usize) {
        if FIRED_N.fetch_add(1, Ordering::SeqCst) < 4 {
            SELF_REARM.lock().unwrap().as_ref().unwrap().arm(0);
        }
    }

    #[test]
    fn a_callback_rearming_in_the_past_is_bounded() {
        let _g = setup();
        FIRED_N.store(0, Ordering::SeqCst);
        let t = HrTimer::new(rearm_self, 0);
        *SELF_REARM.lock().unwrap() = Some(t.clone());
        t.arm(0);
        let mut n = 0;
        while poll_timers() > 0 {
            n += 1;
            assert!(n < 10);
        }
        assert_eq!(FIRED_N.load(Ordering::SeqCst), 5, "fired once plus four re-arms");
        *SELF_REARM.lock().unwrap() = None;
        drop(t);
    }

    static NESTED: Mutex<Option<HrTimer>> = Mutex::new(None);
    static DEPTH: AtomicU32 = AtomicU32::new(0);
    static MAX_DEPTH: AtomicU32 = AtomicU32::new(0);
    /// Re-arms itself in the past and, while still running, lets the polled queue run again (the poller thread
    /// interrupted inside its callback by a queue run that finds the timer due).
    fn nested_run(_: usize) {
        let d = DEPTH.fetch_add(1, Ordering::SeqCst) + 1;
        MAX_DEPTH.fetch_max(d, Ordering::SeqCst);
        if FIRED_N.fetch_add(1, Ordering::SeqCst) == 0 {
            let t = NESTED.lock().unwrap().clone();
            if let Some(t) = t {
                t.arm(0);
                assert!(t.is_running());
                assert_eq!(poll_timers(), 0, "the running timer is not run a second time");
                assert!(t.is_armed(), "it waits in the queue for the run to end");
                assert!(t.is_running(), "the inner queue run did not clear the outer run's claim");
            }
        }
        DEPTH.fetch_sub(1, Ordering::SeqCst);
    }

    #[test]
    fn a_callback_never_runs_twice_at_once() {
        let _g = setup();
        FIRED_N.store(0, Ordering::SeqCst);
        MAX_DEPTH.store(0, Ordering::SeqCst);
        let t = HrTimer::new(nested_run, 0);
        *NESTED.lock().unwrap() = Some(t.clone());
        t.arm(0);
        // The first run, and the re-arm if its 2 us retry is due by the time the run ended.
        assert!((1..=2).contains(&poll_timers()));
        assert!(!t.is_running());
        let end = crate::now_ns() + 1_000_000_000;
        while t.is_armed() && crate::now_ns() < end {
            poll_timers();
        }
        assert!(!t.is_armed(), "the re-arm ran after the first run ended");
        assert_eq!(FIRED_N.load(Ordering::SeqCst), 2);
        assert_eq!(MAX_DEPTH.load(Ordering::SeqCst), 1, "the callback ran inside itself");
        *NESTED.lock().unwrap() = None;
        drop(t);
    }

    #[test]
    fn a_dead_timer_is_never_queued_again() {
        // The adoption of the polled queue holds the timer between two queues while its last handle drops:
        // the drop waits (bounded), gives up, and the late arm is refused.
        let _g = setup();
        let before = count();
        let gave_up = DROP_WAIT_GAVE_UP.get();
        let dead = ARM_DEAD.get();
        let a = HrTimer::new(rec, 3);
        a.arm(u64::MAX);
        let inner = a.h.t.clone();
        drop(a);
        assert_eq!(DROP_WAIT_GAVE_UP.get(), gave_up + 1, "another holder outlived the bounded wait");
        assert_eq!(count(), before);
        arm_inner(&inner, 0);
        assert_eq!(ARM_DEAD.get(), dead + 1);
        assert_eq!(inner.queue.load(Ordering::Acquire), NONE, "not queued after its drop");
        assert_eq!(poll_timers(), 0);
        assert_eq!(polled_len(), 0);
        assert!(ORDER.lock().unwrap().is_empty(), "never fired");
    }

    #[test]
    fn adopting_into_a_queue_that_sends_them_back_ends() {
        // On the host no CPU's timer is live, so every adopted timer goes back to the polled queue (as an arm
        // without room in a live CPU's queue would): the adoption must stop after what it held.
        let _g = setup();
        let a = HrTimer::new(rec, 5);
        let b = HrTimer::new(rec, 6);
        a.arm(u64::MAX - 1);
        b.arm(u64::MAX - 2);
        assert_eq!(adopt_polled(), 2);
        assert_eq!(polled_len(), 2, "both still polled");
        assert!(a.is_armed() && b.is_armed());
    }

    #[test]
    fn an_arm_without_room_is_counted_not_hidden() {
        let _g = setup();
        let a = HrTimer::new(rec, 4);
        let lost_before = NO_ROOM.get();
        // Take the polled queue's room away (it is empty): the arm finds none and must not allocate.
        drop(POLLED.lock().heap.replace_storage(Vec::new()));
        a.arm(0);
        assert!(!a.is_armed());
        assert_eq!(NO_ROOM.get(), lost_before + 1);
        assert!(LOST.load(Ordering::Acquire), "flagged for the poller's event");
        assert_eq!(poll_timers(), 0, "logs hrtimer.lost, fires nothing");
        assert!(!LOST.load(Ordering::Acquire));
        assert!(ensure_room(count()), "room back");
        a.arm(0);
        assert_eq!(polled_len(), 1);
        assert_eq!(poll_timers(), 1);
        assert_eq!(polled_len(), 0);
    }

    #[test]
    fn dropping_the_last_clone_cancels() {
        let _g = setup();
        let before = count();
        let a = HrTimer::new(rec, 9);
        let a2 = a.clone();
        a.arm(0);
        drop(a);
        assert!(a2.is_armed(), "a clone keeps it");
        drop(a2);
        assert_eq!(count(), before);
        assert_eq!(poll_timers(), 0, "cancelled by the drop");
    }
}
