//! SC-2 ktests: the lock-order checker with injected violations (order table, inversion, same class, sleeping
//! or taking a mutex under a spin lock), per-thread held lists across sleeps, contention and statistics between
//! threads, wait-queue and mutex timeouts, no lost wake-ups with interrupts on, IrqSpinLock nesting, and the
//! check that the kernel itself broke no lock rule since boot.

use crate::{Class, JoinHandle, sleep_ms, spawn, yield_now};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::{Completion, IrqSpinLock, Mutex, SpinLock, ViolationKind, WaitQueue, lockdep, stats};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sc.locks.order_injected", Safe, order_injected);
ktest!("sc.locks.inversion_injected", Safe, inversion_injected);
ktest!("sc.locks.sleep_under_spin_injected", Safe, sleep_under_spin_injected);
ktest!("sc.locks.per_thread_held", Safe, per_thread_held);
ktest!("sc.locks.mutex_contention", Safe, mutex_contention);
ktest!("sc.locks.timeouts", Safe, timeouts);
ktest!("sc.locks.wakeups_irq_on", Safe, wakeups_irq_on);
ktest!("sc.locks.irq_nesting", Safe, irq_nesting);
ktest!("sc.locks.stats", Safe, statistics);
ktest!("sc.locks.no_unexpected", Safe, no_unexpected);
ktest!("sc.locks.cond_true_after_prepare", Safe, cond_true_after_prepare);
ktest!("sc.locks.irq_depth_per_thread", Safe, irq_depth_per_thread);

const SEC: u64 = 1_000_000_000;

fn join(h: &JoinHandle, what: &str) -> R {
    kassert!(h.join_timeout(30 * SEC), "{what}: thread {} did not finish within 30 s", h.id());
    Ok(())
}

fn need_checker() -> Option<R> {
    (!lockdep::enabled()).then(|| ktest::skip("release build: the lock-order checker is off"))
}

/// Unexpected count unchanged by `f` (the injected ones are marked, everything else is a bug).
fn no_new_unexpected(f: impl FnOnce() -> R) -> R {
    let before = lockdep::counts().unexpected;
    f()?;
    let after = lockdep::counts().unexpected;
    kassert!(after == before, "{} unexpected lock violations during the test: {}", after - before, unexpected_list());
    Ok(())
}

fn unexpected_list() -> String {
    let mut s = String::new();
    for v in lockdep::list().iter().filter(|v| v.count > v.injected) {
        s += &format!("[{} {}:{} -> {}:{} x{} thread {}] ", v.kind.name(), v.held, v.held_level, v.taking, v.taking_level, v.count - v.injected, v.first_thread);
    }
    s
}

fn has(kind: ViolationKind, held: &str, taking: &str) -> bool {
    lockdep::list().iter().any(|v| v.kind == kind && v.held == held && v.taking == taking && v.injected > 0)
}

/// The event the scheduler emits for a new violation reaches the log. `seq`: taken before the violation was
/// injected (SC-3: the CPU on poll duty may report it before this test asks).
fn event_logged(seq: u64, taking: &str) -> R {
    if klog::min_level() < klog::Level::Info {
        return Ok(());
    }
    crate::report_lock_violations();
    let found = klog::read_since(seq, 10_000).iter().any(|r| r.name == "lock.violation" && r.text.contains(taking));
    kassert!(found, "no ksync lock.violation event naming {taking}");
    Ok(())
}

fn order_injected() -> R {
    if let Some(r) = need_checker() {
        return r;
    }
    // Locks of the real class names (classes are by name), not the real locks.
    let ring = SpinLock::named("klog.ring", ());
    let sched = ksync::IrqSpinLock::named("sched", ());
    let heap = SpinLock::named("heap", ());
    let cfg = SpinLock::named("pci.cfg", ());
    let seq = klog::next_seq();
    no_new_unexpected(|| {
        let (_, n) = lockdep::expect_violations(|| {
            let _r = ring.lock();
            let _s = sched.lock();
        });
        kassert!(n == 1, "klog.ring then sched: {n} violations recorded, want 1");
        kassert!(has(ViolationKind::Order, "klog.ring", "sched"));
        let (_, n) = lockdep::expect_violations(|| {
            let _h = heap.lock();
            let _c = cfg.lock();
        });
        kassert!(n == 1, "heap then pci.cfg: {n}, want 1");
        kassert!(has(ViolationKind::Order, "heap", "pci.cfg"));
        // The table's own order passes.
        let (_, n) = lockdep::expect_violations(|| {
            let _c = cfg.lock();
            let _s = sched.lock();
            let _r = ring.lock();
            let _h = heap.lock();
        });
        kassert!(n == 0, "pci.cfg > sched > klog.ring > heap reported {n} violations");
        event_logged(seq, "sched")
    })
}

fn inversion_injected() -> R {
    if let Some(r) = need_checker() {
        return r;
    }
    static A: SpinLock<()> = SpinLock::named("sc.kt.inv_a", ());
    static B: SpinLock<()> = SpinLock::named("sc.kt.inv_b", ());
    static C1: SpinLock<()> = SpinLock::named("sc.kt.same", ());
    static C2: SpinLock<()> = SpinLock::named("sc.kt.same", ());
    let seq = klog::next_seq();
    no_new_unexpected(|| {
        // a then b in one thread, b then a in another: the ABBA pattern, found without a deadlock.
        let t = spawn("sc-kt-ab", Class::Normal, || {
            let (_, _) = lockdep::expect_violations(|| {
                let _a = A.lock();
                let _b = B.lock();
            });
        })
        .map_err(|e| format!("{e:?}"))?;
        join(&t, "a-then-b thread")?;
        let (_, n) = lockdep::expect_violations(|| {
            let _b = B.lock();
            let _a = A.lock();
        });
        kassert!(n == 1, "b then a after a then b: {n}, want 1 inversion");
        kassert!(has(ViolationKind::Inversion, "sc.kt.inv_b", "sc.kt.inv_a"));
        let (_, n) = lockdep::expect_violations(|| {
            let _x = C1.lock();
            let _y = C2.lock();
        });
        kassert!(n == 1, "two locks of one class nested: {n}, want 1");
        kassert!(has(ViolationKind::SameClass, "sc.kt.same", "sc.kt.same"));
        event_logged(seq, "sc.kt.inv_a")
    })
}

fn sleep_under_spin_injected() -> R {
    if let Some(r) = need_checker() {
        return r;
    }
    static S: SpinLock<()> = SpinLock::named("sc.kt.spin", ());
    static M: Mutex<()> = Mutex::named("sc.kt.mutex", ());
    no_new_unexpected(|| {
        let (_, n) = lockdep::expect_violations(|| {
            let _s = S.lock();
            sleep_ms(1);
        });
        kassert!(n == 1, "sleep with a spin lock held: {n}, want 1");
        kassert!(has(ViolationKind::SleepUnderSpin, "sc.kt.spin", "(sleep)"));
        let (_, n) = lockdep::expect_violations(|| {
            let _s = S.lock();
            let _m = M.lock();
        });
        kassert!(n == 1, "mutex under a spin lock: {n}, want 1");
        kassert!(has(ViolationKind::MutexUnderSpin, "sc.kt.spin", "sc.kt.mutex"));
        // A mutex held across a sleep is fine.
        let (_, n) = lockdep::expect_violations(|| {
            let _m = M.lock();
            sleep_ms(1);
        });
        kassert!(n == 0, "sleeping with a mutex held reported {n}");
        Ok(())
    })
}

/// A thread that sleeps while holding a mutex keeps it on its own held list: another thread on the same CPU
/// taking an outer lock meanwhile is not a violation (a per-CPU list would report one).
fn per_thread_held() -> R {
    if let Some(r) = need_checker() {
        return r;
    }
    static INNER: Mutex<u32> = Mutex::named("sc.kt.held_inner", 0);
    static HOLDING: AtomicBool = AtomicBool::new(false);
    static RELEASE: Completion = Completion::new();
    RELEASE.reset();
    HOLDING.store(false, Ordering::Release);
    // A second run in the same boot finds the first run's count.
    *INNER.lock() = 0;
    let outer = ksync::IrqSpinLock::named("sched", ());
    no_new_unexpected(|| {
        let t = spawn("sc-kt-holder", Class::Normal, || {
            let mut g = INNER.lock();
            *g += 1;
            HOLDING.store(true, Ordering::Release);
            kassert_thread(lockdep::held_count() == 1);
            RELEASE.wait_timeout(10 * SEC);
        })
        .map_err(|e| format!("{e:?}"))?;
        for _ in 0..1000 {
            if HOLDING.load(Ordering::Acquire) {
                break;
            }
            sleep_ms(1);
        }
        kassert!(HOLDING.load(Ordering::Acquire), "holder thread did not take the mutex");
        kassert!(INNER.owner() == Some(t.id()), "owner {:?}, holder is {}", INNER.owner(), t.id());
        kassert!(lockdep::held_count() == 0, "this thread holds {} locks it never took", lockdep::held_count());
        let (_, n) = lockdep::expect_violations(|| drop(outer.lock()));
        kassert!(n == 0, "outer lock taken here counted against the sleeping holder's mutex: {n}");
        RELEASE.complete();
        join(&t, "holder")?;
        kassert!(!THREAD_FAILED.swap(false, Ordering::AcqRel), "held_count in the holder thread was not 1");
        kassert!(INNER.owner().is_none() && *INNER.lock() == 1);
        Ok(())
    })
}

static THREAD_FAILED: AtomicBool = AtomicBool::new(false);
fn kassert_thread(ok: bool) {
    if !ok {
        THREAD_FAILED.store(true, Ordering::Release);
    }
}

/// Four threads fight for one mutex and yield while holding it, so the others really sleep on it.
fn mutex_contention() -> R {
    static M: Mutex<u64> = Mutex::named("sc.kt.contention", 0);
    static BAD_OWNER: AtomicUsize = AtomicUsize::new(0);
    *M.lock() = 0;
    BAD_OWNER.store(0, Ordering::Relaxed);
    let before = stats::class("sc.kt.contention").map(|s| s.contended).unwrap_or(0);
    no_new_unexpected(|| {
        // INT-A2a2: all four on one CPU and released together. Spread over two CPUs on a loaded host, the first
        // could finish its 200 rounds before the others ran, and nothing was contended (loop flake).
        static GO: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
        GO.store(false, Ordering::Release);
        let mut hs = Vec::new();
        for i in 0..4 {
            let o = crate::SpawnOpts { cpu: Some(0), pin: true, ..crate::SpawnOpts::default() };
            hs.push(
                crate::spawn_with("sc-kt-contend", if i == 0 { Class::Interactive } else { Class::Normal }, o, || {
                    let me = crate::current_id();
                    let t = time::now_ns() + 5_000_000_000;
                    while !GO.load(Ordering::Acquire) && time::now_ns() < t {
                        yield_now();
                    }
                    for _ in 0..200 {
                        let mut g = M.lock();
                        if M.owner() != Some(me) {
                            BAD_OWNER.fetch_add(1, Ordering::Relaxed);
                        }
                        *g += 1;
                        yield_now();
                        drop(g);
                        yield_now();
                    }
                })
                .map_err(|e| format!("{e:?}"))?,
            );
        }
        GO.store(true, Ordering::Release);
        for h in &hs {
            join(h, "contender")?;
        }
        kassert!(*M.lock() == 800, "count {}", *M.lock());
        kassert!(BAD_OWNER.load(Ordering::Relaxed) == 0, "owner wrong {} times", BAD_OWNER.load(Ordering::Relaxed));
        kassert!(M.waiters() == 0, "{} waiters left", M.waiters());
        let s = stats::class("sc.kt.contention").ok_or("no statistics for sc.kt.contention")?;
        kassert!(s.contended > before, "no contention counted: {s:?}");
        kassert!(s.wait_max_ticks > 0 && s.hold_max_ticks > 0, "{s:?}");
        Ok(())
    })
}

fn timeouts() -> R {
    static M: Mutex<()> = Mutex::named("sc.kt.timeout", ());
    static HELD: AtomicBool = AtomicBool::new(false);
    static GO: Completion = Completion::new();
    GO.reset();
    HELD.store(false, Ordering::Release);
    no_new_unexpected(|| {
        let wq = WaitQueue::new();
        let t0 = time::now_ns();
        kassert!(!wq.wait_until_timeout(|| false, 5_000_000), "a false condition came true");
        let took = time::now_ns() - t0;
        kassert!((5_000_000..5 * SEC).contains(&took), "5 ms timeout took {took} ns");
        kassert!(wq.waiter_count() == 0, "timed-out waiter left on the queue");
        let c = Completion::new();
        kassert!(!c.wait_timeout(2_000_000));
        c.complete();
        kassert!(c.wait_timeout(0));
        // A mutex held by a sleeping thread: lock_timeout gives up.
        let t = spawn("sc-kt-holder2", Class::Normal, || {
            let _g = M.lock();
            HELD.store(true, Ordering::Release);
            GO.wait_timeout(10 * SEC);
        })
        .map_err(|e| format!("{e:?}"))?;
        for _ in 0..1000 {
            if HELD.load(Ordering::Acquire) {
                break;
            }
            sleep_ms(1);
        }
        let before = stats::class("sc.kt.timeout").map(|s| s.timeouts).unwrap_or(0);
        let t1 = time::now_ns();
        kassert!(M.lock_timeout(10_000_000).is_none(), "got a mutex another thread holds");
        kassert!(time::now_ns() - t1 >= 10_000_000);
        kassert!(stats::class("sc.kt.timeout").map(|s| s.timeouts).unwrap_or(0) == before + 1);
        GO.complete();
        join(&t, "holder")?;
        kassert!(M.lock_timeout(SEC).is_some(), "free mutex not taken");
        Ok(())
    })
}

/// Two threads hand a token back and forth through one wait queue with interrupts on: every hand-over needs
/// a wake that is never lost (each wait is bounded so a lost one fails instead of hanging).
fn wakeups_irq_on() -> R {
    static TURN: AtomicU64 = AtomicU64::new(0);
    static WQ: WaitQueue = WaitQueue::new();
    static LOST: AtomicU64 = AtomicU64::new(u64::MAX);
    static IRQ_OFF: AtomicBool = AtomicBool::new(false);
    const ROUNDS: u64 = 2000;
    TURN.store(0, Ordering::Release);
    LOST.store(u64::MAX, Ordering::Release);
    IRQ_OFF.store(false, Ordering::Release);
    let irq_expected = acpi::irq::enabled();
    let side = |me: u64| {
        move || {
            for i in 0..ROUNDS {
                if irq_expected && !hal::cpu::interrupts_enabled() {
                    IRQ_OFF.store(true, Ordering::Release);
                }
                if !WQ.wait_until_timeout(|| TURN.load(Ordering::Acquire) == 2 * i + me, 5 * SEC) {
                    LOST.store(i, Ordering::Release);
                    WQ.wake_all();
                    return;
                }
                TURN.store(2 * i + me + 1, Ordering::Release);
                WQ.wake_all();
            }
        }
    };
    no_new_unexpected(|| {
        let a = spawn("sc-kt-ping", Class::Normal, side(0)).map_err(|e| format!("{e:?}"))?;
        let b = spawn("sc-kt-pong", Class::Normal, side(1)).map_err(|e| format!("{e:?}"))?;
        join(&a, "ping")?;
        join(&b, "pong")?;
        kassert!(LOST.load(Ordering::Acquire) == u64::MAX, "wake lost in round {}", LOST.load(Ordering::Acquire));
        kassert!(TURN.load(Ordering::Acquire) == 2 * ROUNDS, "turn {}", TURN.load(Ordering::Acquire));
        kassert!(!IRQ_OFF.load(Ordering::Acquire), "a hand-over ran with interrupts off");
        kassert!(WQ.waiter_count() == 0);
        Ok(())
    })
}

/// IrqSpinLocks released in the wrong order keep interrupts off until the last one goes (in a fresh thread,
/// which runs with IF=1 once interrupts are on).
fn irq_nesting() -> R {
    static OK: AtomicU64 = AtomicU64::new(0);
    static A: IrqSpinLock<()> = IrqSpinLock::named("sc.kt.irq_a", ());
    static B: IrqSpinLock<()> = IrqSpinLock::named("sc.kt.irq_b", ());
    if !acpi::irq::enabled() {
        return ktest::skip("interrupts are not on (no IOAPIC/LAPIC set-up)");
    }
    OK.store(0, Ordering::Release);
    no_new_unexpected(|| {
        let t = spawn("sc-kt-irq", Class::Normal, || {
            let mut bits = 0;
            if hal::cpu::interrupts_enabled() {
                bits |= 1;
            }
            let ga = A.lock();
            let gb = B.lock();
            if !hal::cpu::interrupts_enabled() && ksync::irq_lock_depth() == 2 {
                bits |= 2;
            }
            drop(ga);
            if !hal::cpu::interrupts_enabled() && ksync::irq_lock_depth() == 1 {
                bits |= 4;
            }
            drop(gb);
            if hal::cpu::interrupts_enabled() && ksync::irq_lock_depth() == 0 {
                bits |= 8;
            }
            OK.store(bits, Ordering::Release);
        })
        .map_err(|e| format!("{e:?}"))?;
        join(&t, "irq thread")?;
        let b = OK.load(Ordering::Acquire);
        kassert!(b & 1 != 0, "the thread started with interrupts off");
        kassert!(b == 15, "IF/depth wrong (bits {b:#b}: 2 = off with both held, 4 = still off after the first release, 8 = on after the last)");
        Ok(())
    })
}

/// A waiter whose condition turns true between `prepare_to_block` and `block` returns without blocking; it
/// must still be scheduled after its next yield (it used to stay "Blocking" and never ran again).
fn cond_true_after_prepare() -> R {
    static DONE: AtomicUsize = AtomicUsize::new(0);
    static WQ: WaitQueue = WaitQueue::new();
    DONE.store(0, Ordering::Release);
    no_new_unexpected(|| {
        let t = spawn("sc-kt-cond", Class::Normal, || {
            // 1st call (before queueing): false; 2nd (the re-check after `prepare_to_block` + enqueue): true.
            let mut calls = 0;
            WQ.wait_until(|| {
                calls += 1;
                calls >= 2
            });
            DONE.store(1, Ordering::Release);
            for _ in 0..3 {
                yield_now();
            }
            // A Mutex wait and a sleep after it work as well.
            static M: Mutex<u32> = Mutex::named("sc.kt.cond_m", 0);
            *M.lock() += 1;
            sleep_ms(1);
            DONE.store(if calls == 2 { 2 } else { 3 }, Ordering::Release);
        })
        .map_err(|e| format!("{e:?}"))?;
        join(&t, "waiter that yields after an early return")?;
        let d = DONE.load(Ordering::Acquire);
        kassert!(d == 2, "state {d} (1 = hung after the yield, 3 = the condition was not re-checked as expected)");
        kassert!(WQ.waiter_count() == 0, "the waiter stayed on the queue");
        Ok(())
    })
}

/// IrqSpinLock depth is per thread: a thread switched out with one held (a bug, but it must not spread) does
/// not leave the next thread with interrupts off after its own IrqSpinLock.
fn irq_depth_per_thread() -> R {
    static HELD: AtomicBool = AtomicBool::new(false);
    static B_BITS: AtomicU64 = AtomicU64::new(0);
    static A_BITS: AtomicU64 = AtomicU64::new(0);
    static LA: IrqSpinLock<()> = IrqSpinLock::named("sc.kt.irqdepth_a", ());
    static LB: IrqSpinLock<()> = IrqSpinLock::named("sc.kt.irqdepth_b", ());
    if !acpi::irq::enabled() {
        return ktest::skip("interrupts are not on (no IOAPIC/LAPIC set-up)");
    }
    HELD.store(false, Ordering::Release);
    B_BITS.store(0, Ordering::Release);
    A_BITS.store(0, Ordering::Release);
    no_new_unexpected(|| {
        let a = spawn("sc-kt-irqd-a", Class::Normal, || {
            let g = LA.lock();
            HELD.store(true, Ordering::Release);
            // Lets B run while A holds an IrqSpinLock (what the fix keeps from spreading).
            while B_BITS.load(Ordering::Acquire) == 0 {
                yield_now();
            }
            let mut bits = 1;
            if ksync::irq_lock_depth() == 1 && !hal::cpu::interrupts_enabled() {
                bits |= 2;
            }
            drop(g);
            if ksync::irq_lock_depth() == 0 && hal::cpu::interrupts_enabled() {
                bits |= 4;
            }
            A_BITS.store(bits, Ordering::Release);
        })
        .map_err(|e| format!("{e:?}"))?;
        let b = spawn("sc-kt-irqd-b", Class::Normal, || {
            while !HELD.load(Ordering::Acquire) {
                yield_now();
            }
            let mut bits = 1;
            if ksync::irq_lock_depth() == 0 {
                bits |= 2;
            }
            drop(LB.lock());
            if hal::cpu::interrupts_enabled() && ksync::irq_lock_depth() == 0 {
                bits |= 4;
            }
            B_BITS.store(bits, Ordering::Release);
        })
        .map_err(|e| format!("{e:?}"))?;
        join(&b, "thread B")?;
        join(&a, "thread A")?;
        let (ab, bb) = (A_BITS.load(Ordering::Acquire), B_BITS.load(Ordering::Acquire));
        kassert!(bb == 7, "thread B inherited A's IrqSpinLock state (bits {bb:#b}: 2 = depth 0 at start, 4 = interrupts on after its own lock)");
        kassert!(ab == 7, "thread A lost its state across the switch (bits {ab:#b}: 2 = depth 1 and IF off after the yield, 4 = IF on after release)");
        Ok(())
    })
}

/// The real locks' classes have counters and the `locks` report shows them.
fn statistics() -> R {
    let s = stats::class("sched").ok_or("no class sched")?;
    kassert!(s.acquired > 0, "the scheduler lock was never counted: {s:?}");
    kassert!(s.hold_max_ticks > 0);
    // MM-rel change request 1 (INT-A1, SC-rel did not run): a lock the real heap takes on every magazine
    // allocation; the class "heap" is only used by the debug-only `order_injected` fake lock.
    let h = stats::class("heap.mags").ok_or("no class heap.mags")?;
    kassert!(h.acquired > 0, "heap.mags never counted: {h:?}");
    kassert!(stats::tsc_hz() == time::tsc_hz(), "TSC rate not handed to ksync");
    let text = stats::report(&stats::ReportOpts::default());
    kassert!(text.starts_with("locks classes="), "{text}");
    kassert!(text.contains("\nsched sched:770 "), "no sched line in: {text}");
    kassert!(text.contains(if lockdep::enabled() { "checker=on" } else { "checker=off" }));
    Ok(())
}

/// Nothing in the kernel broke a lock rule since boot (every other test's injected ones are marked).
fn no_unexpected() -> R {
    crate::report_lock_violations();
    let c = lockdep::counts();
    kassert!(c.unexpected == 0, "{} lock violations since boot: {}", c.unexpected, unexpected_list());
    Ok(())
}
