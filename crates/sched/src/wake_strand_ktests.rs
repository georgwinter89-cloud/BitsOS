//! A woken thread must get its CPU even when the thread running there never blocks and holds a spin lock most of
//! the time (FS's snapshot experiment, 12.11.: a kernel thread did not come back from `sleep_ns` for 20 s to
//! 3 minutes behind readers that loop over cached files). The sleeper and the busy thread share one CPU: the wake-up
//! arrives in an interrupt, almost always while the lock is held, where the interrupt exit may not preempt.

use crate::{Class, SpawnOpts, current_cpu, spawn_with};
use alloc::format;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Completion, SpinLock};
use ktest::{kassert, ktest};

const MS: u64 = 1_000_000;
/// The busy thread gives up after this, so a stranded sleeper ends the test instead of the boot.
const HOG_NS: u64 = 3_000;
const ROUNDS: u64 = 20;
/// Longer than any QEMU scheduling hiccup, far shorter than the busy thread's bound.
const LATE_NS: u64 = 500 * MS;

fn strand() -> ktest::TestResult {
    let n = crate::online_cpus();
    if n < 2 {
        return ktest::fail("needs two CPUs");
    }
    let cpu = (current_cpu() + 1) % n;
    let _pin = crate::pin_for_now(cpu).ok_or("could not pin")?;
    let stop = Arc::new(AtomicBool::new(false));
    let held = Arc::new(AtomicU64::new(0));
    let (s, h) = (stop.clone(), held.clone());
    let hog = spawn_with("sc-strand-hog", Class::Normal, SpawnOpts { cpu: Some(cpu), pin: true, rt_prio: crate::RT_PRIO_DEFAULT }, move || {
        let lock = SpinLock::named("sc.test.strand", ());
        let until = time::now_ns() + HOG_NS * MS;
        while !s.load(Ordering::Acquire) && time::now_ns() < until {
            let _g = lock.lock();
            let t = time::now_ns();
            while time::now_ns() < t + 20_000 {
                core::hint::spin_loop();
            }
            h.fetch_add(1, Ordering::Relaxed);
        }
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    let deferred0 = crate::DEFERRED_PREEMPTIONS.get();
    let mut worst = 0u64;
    let mut result = Ok(());
    for round in 0..ROUNDS {
        let t0 = time::now_ns();
        crate::sleep_ns(5 * MS);
        let late = time::now_ns().saturating_sub(t0 + 5 * MS);
        worst = worst.max(late);
        if late > LATE_NS {
            result = Err(format!("round {round}: the sleeper came back {} ms late behind a busy thread on CPU {cpu}", late / MS));
            break;
        }
    }
    stop.store(true, Ordering::Release);
    kassert!(hog.join_timeout(10_000 * MS), "the busy thread did not end");
    klog::event!(Info, "sched", "ktest.wake_strand", cpu = cpu, worst_us = worst / 1000, hog_rounds = held.load(Ordering::Relaxed),
        deferred = crate::DEFERRED_PREEMPTIONS.get() - deferred0);
    result
}
ktest!("sc.wake.strand", Safe, strand);

/// The same strand through a wake-up in thread context onto the waker's own CPU (review of 2eef302d, M1): the waker
/// holds a spin lock when it wakes a thread pinned to its CPU. `cpus::kick` set only `need_resched` there (no IPI to
/// itself, no tick armed under the lock), so only a later interrupt (none on a CPU whose tick is off) or the waker's
/// blocking would switch. Judged without relying on interrupts: they are off while the wake-up is made, so the
/// deferral must come from the wake-up itself, and the woken thread must run as the lock drops.
fn strand_own() -> ktest::TestResult {
    let n = crate::online_cpus();
    if n < 2 {
        return ktest::fail("needs two CPUs");
    }
    let cpu = (current_cpu() + 1) % n;
    let _pin = crate::pin_for_now(cpu).ok_or("could not pin")?;
    let go = Arc::new(Completion::new());
    let woke = Arc::new(AtomicBool::new(false));
    let (g, w) = (go.clone(), woke.clone());
    // KernelRt, so the wake-up asks for the CPU over the fair waker at once.
    let sleeper = spawn_with("sc-strand-own", Class::KernelRt, SpawnOpts { cpu: Some(cpu), pin: true, rt_prio: crate::RT_PRIO_DEFAULT }, move || {
        g.wait();
        w.store(true, Ordering::Release);
    })
    .map_err(|e| format!("spawn: {e:?}"))?;
    // It ran at its spawn and waits now.
    crate::sleep_ns(10 * MS);
    let deferred0 = crate::DEFERRED_PREEMPTIONS.get();
    let lock = SpinLock::named("sc.test.strand_own", ());
    let (asked, ran_under_lock) = {
        let _g = lock.lock();
        let _irq = hal::cpu::IrqGuard::new();
        let _ = hal::percpu::take_deferred_resched();
        go.complete();
        (hal::percpu::resched_deferred(), woke.load(Ordering::Acquire))
    };
    let ran_at_drop = woke.load(Ordering::Acquire);
    go.complete();
    kassert!(sleeper.join_timeout(10_000 * MS), "the woken thread did not end");
    klog::event!(Info, "sched", "ktest.wake_strand_own", cpu = cpu, asked = asked, ran_at_drop = ran_at_drop, deferred = crate::DEFERRED_PREEMPTIONS.get() - deferred0);
    kassert!(!ran_under_lock, "switched under a spin lock");
    kassert!(asked, "a wake-up onto this CPU under a spin lock left no deferred reschedule");
    kassert!(ran_at_drop, "the woken KernelRt thread had not run when the waker's lock dropped");
    Ok(())
}
ktest!("sc.wake.strand_own", Safe, strand_own);
