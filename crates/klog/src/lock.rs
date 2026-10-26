//! klog's own locks: tried for a bounded time, taken with this CPU's interrupts masked, and failing at once for
//! a nested user on the CPU that holds them (an NMI or #MC arriving inside klog), so logging can never deadlock
//! or stall for long.
//!
//! The owner tag is the lock: whoever swaps it from 0 to its CPU tag owns the lock, and only then takes the
//! `SpinLock` (uncontended, so it never spins), which merely gives safe access to the data. So a nested user on
//! the owning CPU sees the tag in every moment of a hold, and klog never needs `SpinLock::try_lock` (whose
//! G0 version releases the lock when it fails; change request to SC in plan/notes/rb-RB-1.md).
//!
//! Interrupts are masked for the whole hold (as Linux holds its console and log locks with irqsave): an IRQ
//! handler that logs never finds its own CPU holding a klog lock, so it never spins on a holder that cannot run,
//! and a holder is never preempted. Only NMI/#MC can still nest, see the owner tag and give up at once.
//!
//! RB-klog: every hold is short by rule (one record copied in, or a small batch copied out into memory the
//! reader reserved before; no allocation, no sink, no rendering of other records under the lock), so a waiter
//! waits microseconds. Each lock keeps its longest wait (all waiters, and those that came with interrupts
//! already off: an IRQ handler or an `IrqSpinLock` holder) and its longest hold (`Stats`), for the ktest
//! `rb.klog.smp_storm` and the `log` tool. A waiter that came with interrupts off gives up after
//! `IRQOFF_WAIT_NS` instead of `EMIT_WAIT_NS`: a hold that long means a stuck holder, and its own CPU must not
//! stay deaf for 10 ms.
//!
//! **Fairness (INT-A1e).** A plain test-and-set lock is unfair: on the Mini-PC's 16 CPUs a waiter with
//! interrupts off lost the race again and again and spun deaf for 370 us behind holds of 5 us. So:
//! - A waiter whose caller had **interrupts on** waits with them on and masks them only around each attempt (a
//!   thread may be preempted or interrupted between attempts, so it never holds a place in a queue).
//! - A waiter whose caller had **interrupts off** (it cannot be preempted or moved) takes a ticket and waits for
//!   its turn: `serving` is the front, which alone tries the tag; the front's ticket is passed on the moment it
//!   gets the lock. While the queue is not empty, interrupts-on waiters stand aside (for at most `STARVE_NS`,
//!   then they try like everyone else). So an interrupts-off wait is about one hold per waiter ahead of it.
//! - A ticket is never a promise that blocks forever: a waiter that gives up leaves its ticket behind, and a
//!   front that does not move (the lock seen free without it taking it for `SKIP_FREE_NS`, or no move at all for
//!   `SKIP_NS`: a waiter that gave up, a stopped vCPU) is passed over by the waiters behind it. A passed-over
//!   waiter keeps trying freely, so no waiter ever depends on another one being alive.
//! - `Wait::Never` (panic, NMI) is one attempt as before, queue or not.

use crate::now;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use hal::cpu::IrqGuard;
use ksync::{SpinLock, SpinLockGuard};

/// How long a normal emit tries a lock before it gives the record up (a stuck holder, e.g. a CPU stopped by an
/// NMI); far above any real hold time. Interrupts stay on between the tries.
pub const EMIT_WAIT_NS: u64 = 10_000_000;
/// The same for a caller that came with interrupts already off (its CPU answers no IPI meanwhile).
pub const IRQOFF_WAIT_NS: u64 = 2_000_000;
/// How long a reader (MCP `log`, the drain, `ring_info`, `resize`) tries per chunk before it reports "busy".
pub const READ_WAIT_NS: u64 = 100_000_000;
/// Without a clock (early boot, host tests) a wait is counted in spins instead.
const SPINS_PER_MS: u64 = 1 << 16;
/// An interrupts-on waiter stands aside for queued interrupts-off waiters at most this long, then it tries like
/// everyone else (its own wait stays far below `EMIT_WAIT_NS`; it waits with interrupts on anyway).
pub const STARVE_NS: u64 = 100_000;
/// A queue front that left the lock free this long without taking it is passed over (a live front takes a free
/// lock within a microsecond).
pub const SKIP_FREE_NS: u64 = 20_000;
/// A queue front that did not move at all this long is passed over (its waiter gave up behind a stuck holder,
/// or its vCPU was stopped); far above any real hold.
pub const SKIP_NS: u64 = 1_000_000;
/// Spins between two looks at the clock.
const CHECK_EVERY: u64 = 64;

/// The waiting times as one value (the kernel uses `TUNING`; host tests stretch them so a slow host thread
/// cannot time out or be passed over where the test wants a queue).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tuning {
    pub irqoff_wait: u64,
    pub starve: u64,
    pub skip_free: u64,
    pub skip: u64,
}

pub(crate) const TUNING: Tuning = Tuning { irqoff_wait: IRQOFF_WAIT_NS, starve: STARVE_NS, skip_free: SKIP_FREE_NS, skip: SKIP_NS };

/// This CPU as a non-zero tag (0 = nobody).
#[cfg(target_os = "none")]
pub fn cpu_tag() -> usize {
    hal::percpu::this_cpu() + 1
}

/// Host builds (klog's own tests and every other crate's host tests): every thread is its own "CPU", so threads
/// really contend for the ring lock. With one tag for all (hal's host `this_cpu` is 0) a thread logging while
/// another held the ring looked like a nested NMI and its record was dropped (igc's host tests lost events).
#[cfg(not(target_os = "none"))]
pub fn cpu_tag() -> usize {
    extern crate std;
    static NEXT: AtomicUsize = AtomicUsize::new(1);
    std::thread_local!(static TAG: usize = NEXT.fetch_add(1, Ordering::Relaxed));
    TAG.with(|t| *t)
}

/// Interrupts are on in the caller (host builds: always "on", the host has no interrupt flag to keep).
#[cfg(target_os = "none")]
pub fn irqs_on() -> bool {
    hal::cpu::interrupts_enabled()
}
#[cfg(not(target_os = "none"))]
pub fn irqs_on() -> bool {
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// One try (panic/NMI path).
    Never,
    /// Up to this many ns (normal emit: `EMIT_WAIT_NS`, readers: `READ_WAIT_NS`); capped at `IRQOFF_WAIT_NS`
    /// for a caller with interrupts off.
    Bounded(u64),
}

/// Longest waits and holds of one klog lock since the last `reset` (ns; 0 without a clock).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Longest wait of any caller that got the lock.
    pub wait_max_ns: u64,
    /// Longest wait of a caller that came with interrupts off (spun deaf to IPIs all the time).
    pub irqoff_wait_max_ns: u64,
    /// Longest hold.
    pub hold_max_ns: u64,
    /// Callers that gave up (wait ran out; nested users on the owning CPU are not counted).
    pub timeouts: u64,
    /// Queue fronts passed over (`SKIP_FREE_NS`, `SKIP_NS`): 0 unless a waiter gave up or a vCPU stopped.
    pub skips: u64,
}

/// A klog lock: the owner tag, the data behind a `SpinLock`, the interrupts-off waiters' tickets, and its
/// statistics.
pub struct KLock<T> {
    lock: SpinLock<T>,
    owner: AtomicUsize,
    /// The next ticket an interrupts-off waiter takes.
    next: AtomicU64,
    /// The ticket at the front (`next == serving`: nobody queued). Tickets below it try freely.
    serving: AtomicU64,
    wait_max: AtomicU64,
    irqoff_wait_max: AtomicU64,
    hold_max: AtomicU64,
    timeouts: AtomicU64,
    skips: AtomicU64,
}

enum Attempt<'a, T> {
    Got(Held<'a, T>),
    /// This CPU holds it already: an NMI or #MC nested inside klog.
    Nested,
    Busy,
}

/// One caller's wait: its start, its limit, and the clock (or spins, without one).
struct Waiter {
    start: u64,
    clocked: bool,
    limit: u64,
    spins: u64,
    tune: Tuning,
}

impl Waiter {
    fn elapsed(&self) -> u64 {
        if self.clocked { now().saturating_sub(self.start) } else { self.spins.saturating_mul(1_000_000) / SPINS_PER_MS }
    }
}

impl<T> KLock<T> {
    pub const fn named(name: &'static str, v: T) -> Self {
        KLock {
            lock: SpinLock::named(name, v),
            owner: AtomicUsize::new(0),
            next: AtomicU64::new(0),
            serving: AtomicU64::new(0),
            wait_max: AtomicU64::new(0),
            irqoff_wait_max: AtomicU64::new(0),
            hold_max: AtomicU64::new(0),
            timeouts: AtomicU64::new(0),
            skips: AtomicU64::new(0),
        }
    }

    pub fn stats(&self) -> Stats {
        Stats {
            wait_max_ns: self.wait_max.load(Ordering::Relaxed),
            irqoff_wait_max_ns: self.irqoff_wait_max.load(Ordering::Relaxed),
            hold_max_ns: self.hold_max.load(Ordering::Relaxed),
            timeouts: self.timeouts.load(Ordering::Relaxed),
            skips: self.skips.load(Ordering::Relaxed),
        }
    }

    pub fn reset_stats(&self) {
        for a in [&self.wait_max, &self.irqoff_wait_max, &self.hold_max, &self.timeouts, &self.skips] {
            a.store(0, Ordering::Relaxed);
        }
    }

    /// `None`: this CPU already holds it (nested), or the wait ran out.
    pub fn lock(&self, wait: Wait) -> Option<Held<'_, T>> {
        self.lock_as(wait, irqs_on(), TUNING)
    }

    /// `lock` for a caller whose interrupts were on (`irq_was_on`) or off (host tests play both).
    pub(crate) fn lock_as(&self, wait: Wait, irq_was_on: bool, tune: Tuning) -> Option<Held<'_, T>> {
        let start = now();
        let limit = match wait {
            Wait::Never => {
                return match self.attempt(start, false, irq_was_on) {
                    Attempt::Got(h) => Some(h),
                    _ => None,
                };
            }
            Wait::Bounded(ns) if !irq_was_on => ns.min(tune.irqoff_wait),
            Wait::Bounded(ns) => ns,
        };
        // One attempt first unless someone is queued (then the queue's front is next in line).
        if self.queue_empty() {
            match self.attempt(start, false, irq_was_on) {
                Attempt::Got(h) => return Some(h),
                Attempt::Nested => return None,
                Attempt::Busy => {}
            }
        } else if self.held_here() {
            return None;
        }
        let mut w = Waiter { start, clocked: start != 0, limit, spins: 0, tune };
        if irq_was_on { self.wait_aside(&mut w) } else { self.wait_queued(&mut w) }
    }

    fn queue_empty(&self) -> bool {
        self.serving.load(Ordering::Acquire) == self.next.load(Ordering::Acquire)
    }

    /// This CPU holds the lock (read with interrupts masked: the tag must name the CPU we run on).
    fn held_here(&self) -> bool {
        let _irq = IrqGuard::new();
        self.owner.load(Ordering::Acquire) == cpu_tag()
    }

    /// One attempt with interrupts masked. The CPU tag is read only with them masked: a thread waiting with
    /// interrupts on may be preempted and moved to another CPU between two attempts, and a tag read earlier
    /// would name the old CPU (a false "nested", or a hold under the old CPU's tag that makes every emit there
    /// give up; review INT-A1d core 2).
    fn attempt(&self, start: u64, waited: bool, irq_was_on: bool) -> Attempt<'_, T> {
        let irq = IrqGuard::new();
        let me = cpu_tag();
        let cur = self.owner.load(Ordering::Acquire);
        if cur == me {
            return Attempt::Nested;
        }
        if cur != 0 || self.owner.compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed).is_err() {
            return Attempt::Busy;
        }
        // Only the tag's winner takes the spin lock: free, so this does not spin.
        let g = self.lock.lock();
        let t = if start != 0 { now() } else { 0 };
        if waited && t != 0 {
            let w = t.saturating_sub(start);
            self.wait_max.fetch_max(w, Ordering::Relaxed);
            if !irq_was_on {
                self.irqoff_wait_max.fetch_max(w, Ordering::Relaxed);
            }
        }
        Attempt::Got(Held { g: Some(g), k: self, since: t, irq: Some(irq) })
    }

    /// An interrupts-on caller: attempts with interrupts masked only around each, standing aside while
    /// interrupts-off waiters are queued (at most `STARVE_NS`).
    fn wait_aside(&self, w: &mut Waiter) -> Option<Held<'_, T>> {
        let mut starved = false;
        loop {
            core::hint::spin_loop();
            w.spins += 1;
            if w.spins.is_multiple_of(CHECK_EVERY) {
                let e = w.elapsed();
                if e > w.limit {
                    self.timeouts.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                starved = e > w.tune.starve;
            }
            // Test before test-and-set: waiters read the tag (the line stays shared) and try the exchange only
            // when it looks free, so they do not slow the holder's release by pulling the line away.
            if self.owner.load(Ordering::Relaxed) == 0 && (starved || self.queue_empty()) {
                match self.attempt(w.start, true, true) {
                    Attempt::Got(h) => return Some(h),
                    Attempt::Nested => return None,
                    Attempt::Busy => {}
                }
            }
        }
    }

    /// An interrupts-off caller: a ticket, then attempts only at the front (or once passed over).
    fn wait_queued(&self, w: &mut Waiter) -> Option<Held<'_, T>> {
        let t = self.next.fetch_add(1, Ordering::AcqRel);
        self.wait_ticket(t, w)
    }

    /// The wait for ticket `t`. A ticket that was passed over (`serving > t`) stays valid: its waiter keeps
    /// attempting whenever the lock is free, only without its place in front (host tests draw a ticket and wait
    /// for it later to play a front that was preempted or interrupted between the two).
    fn wait_ticket(&self, t: u64, w: &mut Waiter) -> Option<Held<'_, T>> {
        let mut last = self.serving.load(Ordering::Acquire);
        let mut moved_at = 0u64;
        let mut free_from: Option<u64> = None;
        loop {
            if self.serving.load(Ordering::Acquire) >= t && self.owner.load(Ordering::Relaxed) == 0 {
                match self.attempt(w.start, true, false) {
                    Attempt::Got(h) => {
                        // The front moves on the moment the lock is taken (a passed-over ticket moves nothing).
                        self.serving.fetch_max(t + 1, Ordering::AcqRel);
                        return Some(h);
                    }
                    Attempt::Nested => {
                        self.leave(t);
                        return None;
                    }
                    Attempt::Busy => {}
                }
            }
            core::hint::spin_loop();
            w.spins += 1;
            if !w.spins.is_multiple_of(CHECK_EVERY) {
                continue;
            }
            let e = w.elapsed();
            if e > w.limit {
                self.timeouts.fetch_add(1, Ordering::Relaxed);
                self.leave(t);
                return None;
            }
            let s = self.serving.load(Ordering::Acquire);
            if s != last || s >= t {
                (last, moved_at, free_from) = (s, e, None);
                continue;
            }
            free_from = if self.owner.load(Ordering::Relaxed) == 0 { Some(free_from.unwrap_or(e)) } else { None };
            let stale = e.saturating_sub(moved_at) > w.tune.skip || free_from.is_some_and(|f| e.saturating_sub(f) > w.tune.skip_free);
            if stale && self.serving.compare_exchange(s, s + 1, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                self.skips.fetch_add(1, Ordering::Relaxed);
                (last, moved_at, free_from) = (s + 1, e, None);
            }
        }
    }

    /// A queued waiter gives up: at the front it passes the front on; behind it, its ticket is passed over later.
    fn leave(&self, t: u64) {
        let _ = self.serving.compare_exchange(t, t + 1, Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// A held klog lock that records its CPU, so a nested user on the same CPU fails fast instead of deadlocking.
pub struct Held<'a, T> {
    g: Option<SpinLockGuard<'a, T>>,
    k: &'a KLock<T>,
    /// When the hold began (0 without a clock).
    since: u64,
    irq: Option<IrqGuard>,
}

impl<T> Deref for Held<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.g.as_deref().expect("held until drop")
    }
}
impl<T> DerefMut for Held<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.g.as_deref_mut().expect("held until drop")
    }
}
impl<T> Drop for Held<'_, T> {
    fn drop(&mut self) {
        if self.since != 0 {
            self.k.hold_max.fetch_max(now().saturating_sub(self.since), Ordering::Relaxed);
        }
        // The spin lock first, the tag (the real lock) after it, interrupts last.
        drop(self.g.take());
        self.k.owner.store(0, Ordering::Release);
        drop(self.irq.take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use alloc::vec::Vec;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    /// Nothing times out or is passed over unless a test asks for it.
    const PATIENT: Tuning = Tuning { irqoff_wait: u64::MAX / 4, starve: u64::MAX / 4, skip_free: u64::MAX / 4, skip: u64::MAX / 4 };
    const FOREVER: Wait = Wait::Bounded(u64::MAX / 4);

    fn wait_for(f: impl Fn() -> bool) {
        let t = std::time::Instant::now();
        while !f() {
            assert!(t.elapsed().as_secs() < 20, "condition not reached");
            std::thread::yield_now();
        }
    }

    #[test]
    fn interrupts_off_waiters_get_the_lock_in_arrival_order() {
        let k: Arc<KLock<Vec<usize>>> = Arc::new(KLock::named("t.fifo", Vec::new()));
        let held = k.lock_as(FOREVER, true, PATIENT).unwrap();
        let mut hs = Vec::new();
        for i in 0..6 {
            let k2 = k.clone();
            hs.push(std::thread::spawn(move || {
                let mut g = k2.lock_as(FOREVER, false, PATIENT).expect("got it");
                g.push(i);
            }));
            // The next one comes only once this one holds its ticket.
            wait_for(|| k.next.load(Ordering::Acquire) == i as u64 + 1);
        }
        // An interrupts-on waiter arriving now stands aside for the queue.
        let k2 = k.clone();
        let late = std::thread::spawn(move || {
            let mut g = k2.lock_as(FOREVER, true, PATIENT).expect("got it");
            g.push(99);
        });
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(held);
        for h in hs {
            h.join().unwrap();
        }
        late.join().unwrap();
        let order = k.lock_as(FOREVER, true, PATIENT).unwrap().clone();
        assert_eq!(order, [0, 1, 2, 3, 4, 5, 99]);
        assert!(k.queue_empty());
        assert_eq!(k.stats().timeouts + k.stats().skips, 0);
    }

    #[test]
    fn a_waiter_that_gives_up_does_not_block_the_queue() {
        let k: Arc<KLock<Vec<&str>>> = Arc::new(KLock::named("t.skip", Vec::new()));
        let held = k.lock_as(FOREVER, true, PATIENT).unwrap();
        // A: the front (ticket 0), patient; it keeps the lock until `release` so that C below can only ever see
        // B's dead ticket at the front with the lock free (a front A preempted on a loaded host would look the
        // same to C's 1 us and be passed over too: `a_passed_over_front_still_gets_the_lock` covers that).
        let release = Arc::new(AtomicBool::new(false));
        let (ka, ra) = (k.clone(), release.clone());
        let a = std::thread::spawn(move || {
            let mut g = ka.lock_as(FOREVER, false, PATIENT).unwrap();
            g.push("a");
            wait_for(|| ra.load(Ordering::Acquire));
        });
        wait_for(|| k.next.load(Ordering::Acquire) == 1);
        // B: ticket 1, gives up at once and leaves its ticket behind A.
        let kb = k.clone();
        assert!(std::thread::spawn(move || kb.lock_as(Wait::Bounded(1), false, PATIENT).is_none()).join().unwrap());
        assert_eq!(k.stats().timeouts, 1);
        assert_eq!((k.serving.load(Ordering::Acquire), k.next.load(Ordering::Acquire)), (0, 2), "B's ticket is left");
        drop(held);
        // A takes the lock: the front moves to B's dead ticket.
        wait_for(|| k.owner.load(Ordering::Acquire) != 0 && k.serving.load(Ordering::Acquire) == 1);
        // C: ticket 2, passes the dead ticket over once the lock stands free without it being taken.
        let kc = k.clone();
        let quick = Tuning { skip_free: 1_000, ..PATIENT };
        let c = std::thread::spawn(move || kc.lock_as(FOREVER, false, quick).unwrap().push("c"));
        wait_for(|| k.next.load(Ordering::Acquire) == 3);
        release.store(true, Ordering::Release);
        a.join().unwrap();
        c.join().unwrap();
        assert_eq!(*k.lock_as(FOREVER, true, PATIENT).unwrap(), ["a", "c"]);
        assert_eq!(k.stats().skips, 1);
        assert!(k.queue_empty(), "serving caught up with next");
    }

    #[test]
    fn a_passed_over_front_still_gets_the_lock() {
        // A live front that looks dead (preempted, or interrupted between its ticket and its attempts) is passed
        // over; it must still get the lock afterwards, and nothing it or the others write may be lost.
        let k: Arc<KLock<Vec<&str>>> = Arc::new(KLock::named("t.late", Vec::new()));
        let held = k.lock_as(FOREVER, true, PATIENT).unwrap();
        let ta = k.next.fetch_add(1, Ordering::AcqRel); // A's ticket 0; A does not run yet
        let kc = k.clone();
        let quick = Tuning { skip_free: 1_000, ..PATIENT };
        let c = std::thread::spawn(move || kc.lock_as(FOREVER, false, quick).unwrap().push("c"));
        wait_for(|| k.next.load(Ordering::Acquire) == 2);
        drop(held);
        c.join().unwrap();
        assert_eq!(k.stats().skips, 1, "C passed A's ticket over");
        assert_eq!(k.serving.load(Ordering::Acquire), 2);
        // D takes the lock (the queue is empty again) and holds it while A comes back, so A has to wait as a
        // passed-over ticket.
        let release = Arc::new(AtomicBool::new(false));
        let (kd, rd) = (k.clone(), release.clone());
        let d = std::thread::spawn(move || {
            let mut g = kd.lock_as(FOREVER, false, PATIENT).unwrap();
            g.push("d");
            wait_for(|| rd.load(Ordering::Acquire));
        });
        wait_for(|| k.owner.load(Ordering::Acquire) != 0);
        let ka = k.clone();
        let a = std::thread::spawn(move || {
            let mut w = Waiter { start: 0, clocked: false, limit: u64::MAX / 4, spins: 0, tune: PATIENT };
            ka.wait_ticket(ta, &mut w).expect("a passed-over ticket still gets the lock").push("a");
        });
        release.store(true, Ordering::Release);
        d.join().unwrap();
        a.join().unwrap();
        assert_eq!(*k.lock_as(FOREVER, true, PATIENT).unwrap(), ["c", "d", "a"]);
        assert_eq!(k.serving.load(Ordering::Acquire), 2, "a late ticket does not move the front back");
        assert!(k.queue_empty());
        assert_eq!((k.stats().skips, k.stats().timeouts), (1, 0));
    }

    #[test]
    fn a_front_that_never_moves_is_passed_over() {
        let k: Arc<KLock<u32>> = Arc::new(KLock::named("t.stuck", 0));
        // A ticket nobody will ever use (a waiter whose vCPU stopped), with the lock held meanwhile.
        k.next.fetch_add(1, Ordering::AcqRel);
        let held = k.lock_as(Wait::Never, true, PATIENT).unwrap();
        let k2 = k.clone();
        let stuck_skip = Tuning { skip: 1_000, ..PATIENT };
        let w = std::thread::spawn(move || *k2.lock_as(FOREVER, false, stuck_skip).unwrap() += 1);
        wait_for(|| k.stats().skips == 1);
        drop(held);
        w.join().unwrap();
        assert_eq!(*k.lock_as(FOREVER, true, PATIENT).unwrap(), 1);
    }

    #[test]
    fn mutual_exclusion_with_both_kinds_of_waiters() {
        let k: Arc<KLock<(u64, bool)>> = Arc::new(KLock::named("t.mutex", (0, false)));
        let go = Arc::new(AtomicBool::new(false));
        let hs: Vec<_> = (0..8)
            .map(|i| {
                let (k, go) = (k.clone(), go.clone());
                std::thread::spawn(move || {
                    wait_for(|| go.load(Ordering::Acquire));
                    for _ in 0..2000 {
                        let mut g = k.lock_as(FOREVER, i % 2 == 0, PATIENT).expect("no timeouts");
                        assert!(!g.1, "two holders at once");
                        g.1 = true;
                        g.0 += 1;
                        core::hint::spin_loop();
                        g.1 = false;
                    }
                })
            })
            .collect();
        go.store(true, Ordering::Release);
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(k.lock_as(FOREVER, true, PATIENT).unwrap().0, 16_000);
        assert!(k.queue_empty());
        assert_eq!(k.stats().timeouts, 0);
    }

    #[test]
    fn nested_and_never_keep_their_meaning() {
        let k: KLock<u8> = KLock::named("t.nested", 0);
        let held = k.lock(Wait::Bounded(EMIT_WAIT_NS)).unwrap();
        // The same "CPU" (host: thread) again: nested, at once, whatever the wait and the interrupt state.
        assert!(k.lock_as(Wait::Never, false, TUNING).is_none());
        assert!(k.lock_as(FOREVER, false, PATIENT).is_none());
        assert!(k.lock_as(FOREVER, true, PATIENT).is_none());
        assert!(k.queue_empty(), "a nested caller takes no ticket");
        assert_eq!(k.stats().timeouts, 0);
        drop(held);
        assert!(k.lock(Wait::Never).is_some());
    }
}
