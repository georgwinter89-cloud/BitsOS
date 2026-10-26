//! Host tests (no scheduler installed: waits spin). Every test thread is its own "CPU" here, so the per-CPU
//! bookkeeping and the order checker run per thread. Violations are counted per thread through
//! `expect_violations`, because other tests run in parallel and the global counts move under them.

extern crate std;
use super::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::vec::Vec;

fn threads(n: usize, f: impl Fn(usize) + Send + Sync + 'static) {
    let f = Arc::new(f);
    let hs: Vec<_> = (0..n)
        .map(|i| {
            let f = f.clone();
            std::thread::spawn(move || f(i))
        })
        .collect();
    for h in hs {
        h.join().unwrap();
    }
}

#[test]
fn spinlock_counts_across_threads() {
    let l = Arc::new(SpinLock::named("t.count", 0u64));
    let l2 = l.clone();
    threads(4, move |_| {
        for _ in 0..10_000 {
            *l2.lock() += 1;
        }
    });
    assert_eq!(*l.lock(), 40_000);
    let s = stats::class("t.count").unwrap();
    assert!(s.acquired >= 40_001, "{s:?}");
}

#[test]
fn failed_try_lock_leaves_the_lock_held() {
    // RB-1 change request 7: the failed try used to build (and drop) a guard, releasing the holder's lock.
    let l = SpinLock::new(1);
    let g = l.lock();
    assert!(l.try_lock().is_none());
    assert!(l.is_locked());
    drop(g);
    assert!(!l.is_locked());
    assert!(l.try_lock().is_some());

    let m = Mutex::named("t.try", 5);
    let g = m.lock();
    assert!(m.try_lock().is_none());
    assert!(m.is_locked());
    assert_eq!(m.owner(), Some(thread_id()));
    drop(g);
    assert!(!m.is_locked());
    assert_eq!(m.owner(), None);

    let i = IrqSpinLock::new(3);
    let g = i.lock();
    assert!(i.try_lock().is_none());
    assert!(i.is_locked());
    drop(g);
    assert_eq!(irq_lock_depth(), 0);

    let r = RwSpinLock::new(0);
    let w = r.write();
    assert!(r.try_read().is_none() && r.try_write().is_none());
    assert!(r.is_write_locked());
    drop(w);
    assert!(!r.is_write_locked());
}

#[test]
fn mutex_without_scheduler_spins() {
    let m = Mutex::named("t", 5);
    *m.lock() += 1;
    assert_eq!(*m.lock(), 6);
    let g = m.lock();
    assert!(m.try_lock().is_none());
    drop(g);
    assert!(m.try_lock().is_some());
    assert!(m.lock_timeout(1).is_some());
}

#[test]
fn mutex_contention_across_threads() {
    let m = Arc::new(Mutex::named("t.mutex_contention", 0u64));
    let m2 = m.clone();
    threads(6, move |_| {
        for _ in 0..5_000 {
            let mut g = m2.lock();
            *g += 1;
            assert_eq!(m2.owner(), Some(thread_id()));
        }
    });
    assert_eq!(*m.lock(), 30_000);
}

#[test]
fn completion_and_once() {
    let c = Completion::new();
    assert!(!c.is_done());
    c.complete();
    c.wait();
    assert_eq!(c.count(), 1);
    c.reset();
    assert!(!c.wait_timeout(0));
    let o: Once<u32> = Once::new();
    assert_eq!(*o.get_or_init(|| 3), 3);
    assert_eq!(*o.get_or_init(|| 4), 3);
    assert!(o.is_completed());
}

#[test]
fn once_races_initialise_once() {
    for _ in 0..50 {
        let o: Arc<Once<u64>> = Arc::new(Once::new());
        let calls = Arc::new(AtomicUsize::new(0));
        let (o2, c2) = (o.clone(), calls.clone());
        threads(8, move |i| {
            let v = *o2.get_or_init(|| {
                c2.fetch_add(1, Ordering::SeqCst);
                std::thread::yield_now();
                i as u64 + 100
            });
            assert!((100..108).contains(&v));
        });
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn onceref_has_one_winner() {
    static VALS: [u32; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
    for _ in 0..50 {
        let r: Arc<OnceRef<u32>> = Arc::new(OnceRef::new());
        let wins = Arc::new(AtomicUsize::new(0));
        let (r2, w2) = (r.clone(), wins.clone());
        threads(8, move |i| {
            if r2.set(&VALS[i]) {
                w2.fetch_add(1, Ordering::SeqCst);
            }
            if let Some(v) = r2.get() {
                assert!(*v < 8);
            }
        });
        assert_eq!(wins.load(Ordering::SeqCst), 1);
        assert!(r.get().is_some());
    }
}

#[test]
fn rwlock_basics() {
    let l = RwSpinLock::named("t.rw", 1);
    {
        let a = l.read();
        let b = l.try_read().unwrap();
        assert_eq!(*a + *b, 2);
        assert_eq!(l.readers(), 2);
        assert!(l.try_write().is_none());
    }
    *l.write() = 7;
    assert_eq!(*l.read(), 7);
    assert_eq!(l.readers(), 0);
}

#[test]
fn rwlock_readers_see_whole_writes() {
    let l = Arc::new(RwSpinLock::named("t.rw_pairs", (0u64, 0u64)));
    let stop = Arc::new(AtomicBool::new(false));
    let (l2, s2) = (l.clone(), stop.clone());
    let readers = std::thread::spawn(move || {
        threads(4, move |_| {
            while !s2.load(Ordering::Relaxed) {
                let g = l2.read();
                assert_eq!(g.0, g.1);
            }
        })
    });
    for i in 0..20_000u64 {
        let mut g = l.write();
        g.0 = i;
        g.1 = i;
    }
    stop.store(true, Ordering::Relaxed);
    readers.join().unwrap();
}

#[test]
fn rwlock_writer_gets_in_under_a_stream_of_readers() {
    let l = Arc::new(RwSpinLock::named("t.rw_starve", 0u64));
    let stop = Arc::new(AtomicBool::new(false));
    let (l2, s2) = (l.clone(), stop.clone());
    let readers = std::thread::spawn(move || {
        threads(4, move |_| {
            while !s2.load(Ordering::Relaxed) {
                let _g = l2.read();
                std::hint::spin_loop();
            }
        })
    });
    std::thread::sleep(Duration::from_millis(20));
    let t = Instant::now();
    for _ in 0..200 {
        *l.write() += 1;
    }
    let took = t.elapsed();
    stop.store(true, Ordering::Relaxed);
    readers.join().unwrap();
    assert_eq!(*l.read(), 200);
    assert!(took < Duration::from_secs(10), "writer starved: {took:?}");
}

#[test]
fn irq_spinlock_nests_in_any_release_order() {
    let a = IrqSpinLock::named("t.irq_a", 1);
    let b = IrqSpinLock::named("t.irq_b", 2);
    let ga = a.lock();
    let gb = b.lock();
    assert_eq!(irq_lock_depth(), 2);
    drop(ga);
    assert_eq!(irq_lock_depth(), 1);
    drop(gb);
    assert_eq!(irq_lock_depth(), 0);
    let gt = a.try_lock().unwrap();
    assert_eq!(irq_lock_depth(), 1);
    drop(gt);
    assert_eq!(irq_lock_depth(), 0);
}

#[test]
fn checker_reports_an_order_violation() {
    if !lockdep::enabled() {
        return;
    }
    let ring = SpinLock::named("klog.ring", ());
    let sched = IrqSpinLock::named("sched", ());
    let (_, n) = lockdep::expect_violations(|| {
        let _r = ring.lock();
        let _s = sched.lock();
    });
    assert_eq!(n, 1);
    let v = lockdep::list();
    assert!(v.iter().any(|x| x.kind == ViolationKind::Order && x.held == "klog.ring" && x.taking == "sched" && x.injected > 0), "{v:?}");
    // The right order is fine.
    let (_, n) = lockdep::expect_violations(|| {
        let _s = sched.lock();
        let _r = ring.lock();
    });
    assert_eq!(n, 0);
}

#[test]
fn checker_finds_an_inversion_between_unordered_classes() {
    if !lockdep::enabled() {
        return;
    }
    let a = SpinLock::named("t.inv_a", ());
    let b = Mutex::named("t.inv_b", ());
    let (_, n) = lockdep::expect_violations(|| {
        {
            let _b = b.lock();
            let _a = a.lock();
        }
        let _a = a.lock();
        let _b = b.lock();
    });
    // a then b (the second time) is the inversion; b under the spin lock a is also a mutex-under-spin.
    assert_eq!(n, 2);
    let v = lockdep::list();
    assert!(v.iter().any(|x| x.kind == ViolationKind::Inversion && x.held == "t.inv_a" && x.taking == "t.inv_b"), "{v:?}");
    assert!(v.iter().any(|x| x.kind == ViolationKind::MutexUnderSpin && x.held == "t.inv_a"), "{v:?}");
}

#[test]
fn checker_same_class_and_recursion() {
    if !lockdep::enabled() {
        return;
    }
    let x = SpinLock::named("t.same", ());
    let y = SpinLock::named("t.same", ());
    let r = RwSpinLock::named("t.rec", ());
    let (_, n) = lockdep::expect_violations(|| {
        let _x = x.lock();
        let _y = y.lock();
    });
    assert_eq!(n, 1);
    let (_, n) = lockdep::expect_violations(|| {
        let _a = r.read();
        let _b = r.read();
    });
    assert_eq!(n, 1);
    let v = lockdep::list();
    assert!(v.iter().any(|x| x.kind == ViolationKind::SameClass && x.held == "t.same"));
    assert!(v.iter().any(|x| x.kind == ViolationKind::Recursive && x.held == "t.rec"));
}

#[test]
fn checker_accepts_the_desk_order_and_out_of_order_release() {
    if !lockdep::enabled() {
        return;
    }
    let s = IrqSpinLock::named("sched", ());
    let e = Mutex::named("pci.enable", ());
    let c = SpinLock::named("pci.cfg", ());
    let d = SpinLock::named("pci.devices", ());
    let k = SpinLock::named("klog.ring", ());
    let h = SpinLock::named("heap", ());
    let f = SpinLock::named("frames", ());
    let un = SpinLock::new(());
    let (_, n) = lockdep::expect_violations(|| {
        let ge = e.lock();
        let gc = c.lock();
        let gd = d.lock();
        let gk = k.lock();
        let gu = un.lock();
        assert_eq!(lockdep::held_count(), 5);
        drop(gc);
        drop(gk);
        let gh = h.lock();
        let gf = f.lock();
        drop(gh);
        drop(gf);
        drop(gd);
        drop(gu);
        drop(ge);
        let _s = s.lock();
        let _k = k.lock();
    });
    assert_eq!(n, 0);
    assert_eq!(lockdep::held_count(), 0);
}

#[test]
fn no_allocation_under_an_allocator_lock() {
    // While an allocator-class lock is held, a never-seen class is not created (it would allocate): the lock
    // counts under (pending) and gets its class the next time.
    let heap = SpinLock::named("heap", ());
    let fresh = SpinLock::named("t.fresh_under_heap", ());
    let (_, n) = lockdep::expect_violations(|| {
        let _h = heap.lock();
        let _f = fresh.lock();
    });
    if lockdep::enabled() {
        // Taking a subsystem lock under the heap is itself against the order... but its class is (pending),
        // which is not checked, so nothing is recorded.
        assert_eq!(n, 0);
    }
    assert!(stats::class("t.fresh_under_heap").is_none());
    // A violation found under the heap lock still gets its record (from the spares).
    if lockdep::enabled() {
        let sched = IrqSpinLock::named("sched", ());
        let (_, n) = lockdep::expect_violations(|| {
            let _h = heap.lock();
            let _s = sched.lock();
        });
        assert_eq!(n, 1);
        assert!(lockdep::list().iter().any(|v| v.kind == ViolationKind::Order && v.held == "heap" && v.taking == "sched"));
        assert_eq!(lockdep::counts().unrecorded, 0);
    }
    drop(fresh.lock());
    assert!(stats::class("t.fresh_under_heap").is_some());
}

#[test]
fn statistics_count_contention() {
    let l = Arc::new(SpinLock::named("t.contended", 0u64));
    let go = Arc::new(AtomicU64::new(0));
    let (l2, g2) = (l.clone(), go.clone());
    threads(4, move |_| {
        g2.fetch_add(1, Ordering::SeqCst);
        while g2.load(Ordering::SeqCst) < 4 {}
        for _ in 0..20_000 {
            let mut g = l2.lock();
            *g += 1;
            std::hint::spin_loop();
        }
    });
    let s = stats::class("t.contended").unwrap();
    assert_eq!(s.acquired, 80_000);
    assert!(s.contended > 0, "{s:?}");
    assert!(s.wait_ticks > 0 && s.wait_max_ticks > 0 && s.hold_max_ticks > 0);
    let text = stats::report(&stats::ReportOpts { prefix: Some("t.contended".into()), ..Default::default() });
    assert!(text.starts_with("locks classes=1 shown=1"), "{text}");
    assert!(text.contains("\nt.contended subsystem:500 80000 "), "{text}");
    let all = stats::report(&stats::ReportOpts { all: true, sort: stats::SortBy::Name, max: 0, prefix: None });
    assert!(all.contains("\nframes alloc:910 ") && all.contains("\nsched sched:770 "), "{all}");
}

#[test]
fn many_dynamic_classes() {
    // R1: no class limit. Leaked names are fine in a test.
    let mut locks = Vec::new();
    for i in 0..300 {
        let name: &'static str = std::boxed::Box::leak(std::format!("t.many.{i}").into_boxed_str());
        locks.push(SpinLock::named(name, i));
    }
    for l in &locks {
        drop(l.lock());
    }
    let n = stats::classes().iter().filter(|c| c.name.starts_with("t.many.")).count();
    assert_eq!(n, 300);
}

#[test]
fn deep_nesting_grows_the_held_list() {
    let locks: Vec<SpinLock<()>> = (0..100).map(|_| SpinLock::new(())).collect();
    let gs: Vec<_> = locks.iter().map(|l| l.lock()).collect();
    if lockdep::enabled() {
        assert_eq!(lockdep::held_count(), 100);
    }
    drop(gs);
    assert_eq!(lockdep::held_count(), 0);
}

#[test]
fn irq_state_follows_the_thread_across_a_switch() {
    let a = IrqSpinLock::named("t.irq_sw_a", ());
    let b = IrqSpinLock::named("t.irq_sw_b", ());
    let ga = a.lock();
    assert_eq!(irq_lock_depth(), 1);
    // sched::schedule around switch_to: the next thread starts with depth 0 and pairs its own locks.
    let saved = take_irq_state();
    assert_eq!(irq_lock_depth(), 0);
    let gb = b.lock();
    assert_eq!(irq_lock_depth(), 1);
    drop(gb);
    assert_eq!(irq_lock_depth(), 0);
    put_irq_state(saved);
    assert_eq!(irq_lock_depth(), 1);
    drop(ga);
    assert_eq!(irq_lock_depth(), 0);
}

#[test]
fn unpaired_irq_release_leaves_depth_at_zero() {
    cpu::irq_restore(cpu::IrqSave::Counted);
    assert_eq!(irq_lock_depth(), 0);
    let a = IrqSpinLock::named("t.irq_unpaired", ());
    let g = a.lock();
    assert_eq!(irq_lock_depth(), 1);
    drop(g);
    assert_eq!(irq_lock_depth(), 0);
}

#[test]
fn nested_save_restore_inside_a_release_keeps_the_word() {
    // What an NMI taking an IrqSpinLock does to the word: a full save/restore pair. Before and after any such
    // pair the word must be the same, at every depth.
    let outer = IrqSpinLock::named("t.irq_nmi_outer", ());
    let g = outer.lock();
    let before = take_irq_state();
    put_irq_state(before);
    let s = cpu::irq_save();
    cpu::irq_restore(s);
    assert_eq!(take_irq_state(), before);
    put_irq_state(before);
    drop(g);
    assert_eq!(irq_lock_depth(), 0);
}

#[test]
fn timed_wait_without_a_clock_is_bounded() {
    wait::NO_CLOCK.with(|c| c.set(true));
    let before = clockless_timeouts();
    let wq = WaitQueue::new();
    let t0 = Instant::now();
    assert!(!wq.wait_until_timeout(|| false, 2_000_000));
    // Generous: only "it ends" is the point (timing is never judged precisely).
    assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());
    assert!(clockless_timeouts() > before);
    // A held mutex: lock_timeout gives up, also without a clock.
    let m = Arc::new(Mutex::named("t.clockless", 0));
    let (m2, held, go) = (m.clone(), Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
    let (held2, go2) = (held.clone(), go.clone());
    let holder = std::thread::spawn(move || {
        let _g = m2.lock();
        held2.store(true, Ordering::Release);
        while !go2.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    });
    while !held.load(Ordering::Acquire) {
        std::thread::yield_now();
    }
    assert!(m.lock_timeout(1_000_000).is_none());
    go.store(true, Ordering::Release);
    holder.join().unwrap();
    assert!(m.lock_timeout(1_000_000).is_some());
    wait::NO_CLOCK.with(|c| c.set(false));
}

#[test]
fn wake_with_nobody_waiting_and_counted_waiters() {
    let wq = WaitQueue::new();
    wq.wake_one();
    wq.wake_all();
    assert_eq!(wq.waiter_count(), 0);
    // A completion completed before and after waiters: every waiter returns (no hooks: they spin).
    let c = Arc::new(Completion::new());
    let c2 = c.clone();
    threads(4, move |i| {
        if i == 0 {
            c2.complete();
        } else {
            c2.wait();
        }
    });
    assert!(c.is_done());
}

#[test]
fn spin_locks_count_in_the_preempt_count() {
    std::thread::spawn(|| {
        let a = SpinLock::named("t.pc.a", ());
        let b = IrqSpinLock::named("t.pc.b", ());
        let r = RwSpinLock::named("t.pc.r", ());
        let m = Mutex::named("t.pc.m", ());
        assert_eq!(preempt_count(), 0);
        {
            let _a = a.lock();
            assert_eq!(preempt_count(), 1);
            let _b = b.lock();
            let _r = r.read();
            assert_eq!(preempt_count(), 3);
            // A sleeping mutex does not hold off preemption.
            let _m = m.lock();
            assert_eq!(preempt_count(), 3);
        }
        assert_eq!(preempt_count(), 0);
        // A failed try leaves nothing counted; a successful one counts until released.
        let held = a.lock();
        assert!(a.try_lock().is_none());
        assert_eq!(preempt_count(), 1);
        drop(held);
        let w = r.try_write();
        assert!(w.is_some() && preempt_count() == 1);
        drop(w);
        {
            let _g = PreemptGuard::new();
            assert_eq!(preempt_count(), 1);
        }
        assert_eq!(preempt_count(), 0);
    })
    .join()
    .unwrap();
}

#[test]
fn preempt_count_follows_the_thread_across_a_switch() {
    std::thread::spawn(|| {
        let a = SpinLock::named("t.pc.switch", ());
        let g = a.lock();
        // sched: take before switch_to, put on the CPU the thread runs on afterwards.
        let saved = take_irq_state();
        assert_eq!(preempt_count(), 0, "the next thread starts unblocked");
        put_irq_state(saved);
        assert_eq!(preempt_count(), 1);
        drop(g);
        assert_eq!(preempt_count(), 0);
    })
    .join()
    .unwrap();
}

#[test]
fn wake_all_from_a_handler_frees_nothing() {
    // The queue keeps its memory after wake_all (an interrupt handler must not free); waiters queue again.
    let wq = Arc::new(WaitQueue::new());
    let flag = Arc::new(AtomicBool::new(false));
    let (w, f) = (wq.clone(), flag.clone());
    let h = std::thread::spawn(move || w.wait_until_timeout(|| f.load(Ordering::Acquire), 2_000_000_000));
    std::thread::sleep(Duration::from_millis(20));
    flag.store(true, Ordering::Release);
    wq.wake_all();
    assert!(h.join().unwrap());
    assert_eq!(wq.waiter_count(), 0);
}
