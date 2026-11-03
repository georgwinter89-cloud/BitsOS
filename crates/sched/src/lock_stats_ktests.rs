//! PF-2 ktests: lock statistics per CPU. N threads, one per CPU, each take their own lock of one class; the
//! class counts every acquisition exactly once, and (since PF-2) each CPU counted in its own slot, so taking a
//! lock wrote no cache line another CPU wrote. The ticks per lock/unlock go into an event for the notes (QEMU
//! numbers are never judged).

use crate::{Class, SpawnOpts, spawn_with, yield_now};
use alloc::format;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{SpinLock, stats};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sc.locks.percpu_stats", Safe, percpu_stats);

const ROUNDS: u64 = 100_000;
const CLASS: &str = "sc.kt.percpu";

/// Each lock on its own cache lines, so only the statistics can share a line between the CPUs.
#[repr(align(128))]
struct Padded(SpinLock<u64>);

struct Run {
    locks: Vec<Padded>,
    ticks: Vec<AtomicU64>,
    go: AtomicBool,
}

fn percpu_stats() -> R {
    let n = hal::percpu::cpu_count().max(1);
    let run = Arc::new(Run {
        locks: (0..n).map(|_| Padded(SpinLock::named(CLASS, 0))).collect(),
        ticks: (0..n).map(|_| AtomicU64::new(0)).collect(),
        go: AtomicBool::new(false),
    });
    let before = stats::class(CLASS).map_or(0, |s| s.acquired);
    let mut hs = Vec::new();
    for cpu in 0..n {
        let r = run.clone();
        let o = SpawnOpts { cpu: Some(cpu), pin: true, ..SpawnOpts::default() };
        let h = spawn_with("sc-kt-percpu", Class::Normal, o, move || {
            let t = time::now_ns() + 5_000_000_000;
            while !r.go.load(Ordering::Acquire) && time::now_ns() < t {
                yield_now();
            }
            let l = &r.locks[cpu].0;
            let t0 = hal::cpu::rdtsc();
            for _ in 0..ROUNDS {
                *l.lock() += 1;
            }
            r.ticks[cpu].store(hal::cpu::rdtsc().wrapping_sub(t0), Ordering::Release);
        })
        .map_err(|e| format!("{e:?}"))?;
        hs.push(h);
    }
    run.go.store(true, Ordering::Release);
    for h in &hs {
        kassert!(h.join_timeout(60_000_000_000), "thread {} did not finish within 60 s", h.id());
    }
    for (i, l) in run.locks.iter().enumerate() {
        let v = *l.0.lock();
        kassert!(v == ROUNDS, "lock {i} counted {v} rounds");
    }
    let s = stats::class(CLASS).ok_or("no statistics for sc.kt.percpu")?;
    // The check above took every lock once more.
    let want = n as u64 * (ROUNDS + 1);
    kassert!(s.acquired - before == want, "acquired grew by {}, want {want}: {s:?}", s.acquired - before);
    // PF-2: each pinned thread counted in its own CPU's slot. With several CPUs the boot gave every class slots
    // for them all (`ksync::prepare_cpus` on the multi-CPU path), and a class made later covers them at
    // creation, so a missing slot is a failure. One CPU (smp=off) may count in the shared slot, which is
    // correct there (nobody to share a line with); the event says whether the check ran (`slots_checked`).
    let sp = stats::spread(CLASS).ok_or("no spread for sc.kt.percpu")?;
    kassert!(n == 1 || sp.covered >= n, "{n} CPUs but the class has slots for only {}: {sp:?}", sp.covered);
    let checked = sp.covered >= n;
    if checked {
        let own = (0..n).filter(|c| sp.cpus.iter().any(|&(i, a)| i == *c && a >= ROUNDS)).count();
        kassert!(own == n, "only {own} of {n} CPUs counted in their own slot: {sp:?}");
    }
    let per: Vec<u64> = run.ticks.iter().map(|t| t.load(Ordering::Acquire) / ROUNDS).collect();
    let max = per.iter().copied().max().unwrap_or(0);
    let avg = per.iter().sum::<u64>() / n as u64;
    klog::event!(Info, "sched", "ktest.lock_stats_percpu", cpus = n, rounds = ROUNDS, ticks_per_lock_avg = avg, ticks_per_lock_max = max,
        covered = sp.covered, shared = sp.shared, slots_checked = checked);
    Ok(())
}
