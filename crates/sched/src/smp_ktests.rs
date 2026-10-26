//! SC-3 ktests: every CPU schedules, 64 busy threads scale with every thread advancing and every wake-up
//! arriving, the class rules (KernelRt strict and FIFO, fair weights, the Interactive latency bonus), wake-ups
//! from an interrupt handler (self-IPI), balancing that moves work, the reaper's sync generations, and the
//! interrupt-exit preemption hook. Timing is never judged under QEMU: the limits there are generous and
//! functional; the Mini-PC numbers come from the events (`sched ktest.*`).

use crate::{Class, JoinHandle, SpawnOpts, cond_resched, current_cpu, spawn_with};
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::{Completion, SpinLock, WaitQueue};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sched.cpus.joined", Safe, cpus_joined);
ktest!("sched.scale64", Safe, scale64);
ktest!("sched.class.rt_strict", Safe, rt_strict);
ktest!("sched.class.rt_fifo", Safe, rt_fifo);
ktest!("sched.class.fair_weights", Safe, fair_weights);
ktest!("sched.class.interactive_wake", Safe, interactive_wake);
ktest!("sched.wake.irq", Safe, wake_irq);
ktest!("sched.balance.spread", Safe, balance_spread);
ktest!("sched.reap.sync", Safe, reap_sync);
ktest!("sched.preempt.hook", Safe, preempt_hook);
ktest!("sched.exclusive", Safe, exclusive);
ktest!("sched.latency.rt_under_load", Safe, rt_latency);

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

fn hypervisor() -> bool {
    hal::cpu::live().has("hypervisor")
}

fn spawn_on(name: &str, class: Class, cpu: Option<usize>, pin: bool, rt_prio: u8, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, String> {
    spawn_with(name, class, SpawnOpts { cpu, pin, rt_prio }, f).map_err(|e| format!("spawn {name}: {e:?}"))
}

fn join_all(hs: &[JoinHandle], what: &str) -> R {
    for h in hs {
        kassert!(h.join_timeout(30 * SEC), "{what}: thread {} did not finish within 30 s", h.id());
    }
    Ok(())
}

/// Busy work that cannot be optimised away.
fn spin(n: u32) {
    let mut x = 0u64;
    for i in 0..n {
        x = core::hint::black_box(x.wrapping_mul(31).wrapping_add(i as u64));
    }
    core::hint::black_box(x);
}

/// A CPU other than the caller's for the class tests (the last online one), and the caller pinned elsewhere so
/// it is never stuck behind the test's threads (until the guard drops). None with one CPU.
fn test_cpu() -> Option<(usize, crate::PinGuard)> {
    let n = crate::wait_cpus_online(5 * SEC);
    if n < 2 {
        return None;
    }
    let target = (0..crate::cpus::count()).rev().find(|&c| crate::cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire)))?;
    let home = (0..crate::cpus::count()).find(|&c| c != target && crate::cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire)))?;
    crate::pin_for_now(home).map(|g| (target, g))
}

/// Every CPU that is up joined the scheduler, has an idle thread, and runs a thread pinned to it.
fn cpus_joined() -> R {
    let up = hal::percpu::cpu_count();
    let online = crate::wait_cpus_online(5 * SEC);
    kassert!(online == up, "{online} of {up} CPUs schedule");
    kassert!(crate::cpus::resched_vector() != 0, "no reschedule vector");
    let seen = Arc::new(SpinLock::new(Vec::new()));
    let mut hs = Vec::new();
    for c in 0..up {
        let cs = crate::cpus::of(c).ok_or("no CPU state")?;
        kassert!(cs.idle.get().is_some(), "CPU {c} has no idle thread");
        let s = seen.clone();
        hs.push(spawn_on("sched-pin", Class::Normal, Some(c), true, crate::RT_PRIO_DEFAULT, move || s.lock().push((c, current_cpu())))?);
    }
    join_all(&hs, "pinned threads")?;
    let v = seen.lock();
    kassert!(v.len() == up, "{} of {up} pinned threads ran", v.len());
    for &(want, got) in v.iter() {
        kassert!(want == got, "a thread pinned to CPU {want} ran on {got}");
    }
    klog::event!(Info, "sched", "ktest.cpus", online = online, vector = klog::Hex(crate::cpus::resched_vector() as u64));
    Ok(())
}

/// 64 busy kernel threads on every CPU: rounds of wake-ups that must all arrive, then a busy phase in which every
/// thread advances in every window and every CPU is busy (> 90 % on hardware; QEMU: functional).
fn scale64() -> R {
    const N: usize = 64;
    const ROUNDS: u64 = 20;
    let ncpu = crate::wait_cpus_online(5 * SEC).max(1);
    let counters: Arc<Vec<AtomicU64>> = Arc::new((0..N).map(|_| AtomicU64::new(0)).collect());
    let round = Arc::new(AtomicU64::new(0));
    let arrived = Arc::new(AtomicU64::new(0));
    let busy = Arc::new(AtomicBool::new(false));
    let stop = Arc::new(AtomicBool::new(false));
    let wq = Arc::new(WaitQueue::new());
    let missed = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::with_capacity(N);
    for i in 0..N {
        let (counters, round, arrived, busy, stop, wq, missed) = (counters.clone(), round.clone(), arrived.clone(), busy.clone(), stop.clone(), wq.clone(), missed.clone());
        hs.push(spawn_on("sched-scale", Class::Normal, None, false, crate::RT_PRIO_DEFAULT, move || {
            for r in 1..=ROUNDS {
                // A lost wake-up shows as a 20 s wait; the driver gives up after 10 s.
                if !wq.wait_until_timeout(|| round.load(Ordering::Acquire) >= r, 20 * SEC) {
                    missed.fetch_add(1, Ordering::Relaxed);
                }
                spin(20_000);
                arrived.fetch_add(1, Ordering::AcqRel);
            }
            while !busy.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                crate::sleep_ms(1);
            }
            while !stop.load(Ordering::Acquire) {
                counters[i].fetch_add(1, Ordering::Relaxed);
                spin(2_000);
                cond_resched();
            }
        })?);
    }
    let result = (|| -> R {
        // Phase 1: wake-ups. Each round wakes all 64 sleepers at once; all must arrive.
        for r in 1..=ROUNDS {
            let t0 = time::now_ns();
            while (wq.waiter_count() as u64) < N as u64 - missed.load(Ordering::Relaxed) && time::now_ns() - t0 < 10 * SEC {
                crate::sleep_ms(1);
            }
            round.store(r, Ordering::Release);
            wq.wake_all();
            while arrived.load(Ordering::Acquire) < r * N as u64 && time::now_ns() - t0 < 10 * SEC {
                crate::sleep_ms(1);
            }
            let a = arrived.load(Ordering::Acquire);
            kassert!(a == r * N as u64, "round {r}: {} of {N} threads woke up within 10 s", a - (r - 1) * N as u64);
        }
        kassert!(missed.load(Ordering::Relaxed) == 0, "{} waits timed out", missed.load(Ordering::Relaxed));
        // Phase 2: busy. Windows long enough for every thread to get a slice on a loaded CPU.
        busy.store(true, Ordering::Release);
        let per_cpu = N.div_ceil(ncpu) as u64;
        let window = (3 * crate::SLICE_NS * per_cpu).max(300 * MS);
        crate::sleep_ns(window / 2);
        let stats0 = crate::cpu_stats();
        let t0 = time::now_ns();
        let mut last: Vec<u64> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        for w in 0..3 {
            crate::sleep_ns(window);
            let now: Vec<u64> = counters.iter().map(|c| c.load(Ordering::Relaxed)).collect();
            let stuck: Vec<usize> = (0..N).filter(|&i| now[i] == last[i]).collect();
            // What the scheduler thinks of them (state, CPU, virtual runtime, migrations) against their CPU's queue.
            let why: Vec<String> = stuck
                .iter()
                .map(|&i| {
                    let t = hs[i].thread();
                    let c = t.cpu.load(Ordering::Relaxed);
                    let (min, q) = crate::cpus::of(c).map_or((0, 0), |cs| (cs.rq.lock().min_vruntime, cs.nr_queued.load(Ordering::Relaxed)));
                    format!("{i}: {:?} cpu={c} v={} min={min} queued={q} migr={}", t.state(), t.vruntime.load(Ordering::Relaxed), t.migrations())
                })
                .collect();
            kassert!(stuck.is_empty(), "window {w}: threads {stuck:?} did not advance in {} ms (starved): {why:?}", window / MS);
            last = now;
        }
        let wall = time::now_ns() - t0;
        let stats1 = crate::cpu_stats();
        let mut pct = Vec::new();
        for (a, b) in stats0.iter().zip(stats1.iter()).filter(|(a, _)| a.online) {
            pct.push((a.cpu, b.busy_ns.saturating_sub(a.busy_ns) * 100 / wall.max(1)));
        }
        klog::event!(Info, "sched", "ktest.scale64", cpus = ncpu, threads = N, window_ms = window / MS,
            busy_pct = format!("{pct:?}"), migrations = stats1.iter().map(|s| s.migrations_in).sum::<u64>());
        let low: Vec<(usize, u64)> = pct.iter().copied().filter(|&(_, p)| p <= 90).collect();
        if hypervisor() {
            // QEMU: only functional (every CPU ran test threads at all).
            let idle: Vec<usize> = pct.iter().filter(|&&(_, p)| p == 0).map(|&(c, _)| c).collect();
            kassert!(idle.is_empty(), "CPUs {idle:?} ran nothing in the busy phase");
        } else {
            kassert!(low.is_empty(), "CPUs below 90 % busy: {low:?}");
        }
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    round.store(u64::MAX, Ordering::Release);
    wq.wake_all();
    let j = join_all(&hs, "scale64");
    drop(hs);
    crate::reap();
    result?;
    j
}

/// A KernelRt thread on a CPU keeps a Normal thread there from running at all until it is done.
fn rt_strict() -> R {
    let Some((t, _pin)) = test_cpu() else { return ktest::skip("needs two CPUs") };
    let stop = Arc::new(AtomicBool::new(false));
    let n_count = Arc::new(AtomicU64::new(0));
    let (s, c) = (stop.clone(), n_count.clone());
    let hog = spawn_on("sched-normal", Class::Normal, Some(t), true, 0, move || {
        while !s.load(Ordering::Acquire) {
            c.fetch_add(1, Ordering::Relaxed);
            spin(500);
            cond_resched();
        }
    })?;
    let result = (|| -> R {
        let t0 = time::now_ns();
        while n_count.load(Ordering::Relaxed) == 0 && time::now_ns() - t0 < 5 * SEC {
            crate::sleep_ms(1);
        }
        kassert!(n_count.load(Ordering::Relaxed) > 0, "the Normal thread never ran");
        let out = Arc::new(SpinLock::new(None));
        let (o, c) = (out.clone(), n_count.clone());
        let rt = spawn_on("sched-rt", Class::KernelRt, Some(t), true, 50, move || {
            let before = c.load(Ordering::Relaxed);
            let t0 = time::now_ns();
            let mut loops = 0u64;
            while time::now_ns() - t0 < 100 * MS {
                spin(500);
                cond_resched();
                loops += 1;
            }
            *o.lock() = Some((c.load(Ordering::Relaxed) - before, loops, current_cpu()));
        })?;
        join_all(core::slice::from_ref(&rt), "rt thread")?;
        let (ran, loops, cpu) = out.lock().take().ok_or("no result")?;
        kassert!(cpu == t, "the KernelRt thread ran on CPU {cpu}, pinned to {t}");
        kassert!(ran == 0, "the Normal thread advanced {ran} times while the KernelRt thread ran ({loops} loops)");
        let after = n_count.load(Ordering::Relaxed);
        crate::sleep_ms(20);
        kassert!(n_count.load(Ordering::Relaxed) > after, "the Normal thread did not run again afterwards");
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    let j = join_all(core::slice::from_ref(&hog), "normal hog");
    result?;
    j
}

/// KernelRt FIFO: B (same priority, queued behind A) starts only after A ended; C (higher priority) preempts A at
/// its next preemption point; A, preempted, runs again before B.
fn rt_fifo() -> R {
    let Some((t, _pin)) = test_cpu() else { return ktest::skip("needs two CPUs") };
    let seq = Arc::new(AtomicU64::new(0));
    let log = Arc::new(SpinLock::new(Vec::new()));
    let a_started = Arc::new(AtomicBool::new(false));
    let note = |log: &Arc<SpinLock<Vec<(&'static str, u64)>>>, seq: &Arc<AtomicU64>, what: &'static str| {
        let n = seq.fetch_add(1, Ordering::AcqRel);
        log.lock().push((what, n));
    };
    let busy_ms = |ms: u64| {
        let t0 = time::now_ns();
        while time::now_ns() - t0 < ms * MS {
            spin(500);
            cond_resched();
        }
    };
    let (l, s, st) = (log.clone(), seq.clone(), a_started.clone());
    let a = spawn_on("sched-fifo-a", Class::KernelRt, Some(t), true, 50, move || {
        note(&l, &s, "a.start");
        st.store(true, Ordering::Release);
        busy_ms(60);
        note(&l, &s, "a.end");
    })?;
    let t0 = time::now_ns();
    while !a_started.load(Ordering::Acquire) && time::now_ns() - t0 < 5 * SEC {
        crate::sleep_ms(1);
    }
    let (l, s) = (log.clone(), seq.clone());
    let b = spawn_on("sched-fifo-b", Class::KernelRt, Some(t), true, 50, move || {
        note(&l, &s, "b.start");
        busy_ms(5);
        note(&l, &s, "b.end");
    })?;
    crate::sleep_ms(5);
    let (l, s) = (log.clone(), seq.clone());
    let c = spawn_on("sched-fifo-c", Class::KernelRt, Some(t), true, 60, move || {
        note(&l, &s, "c.start");
        busy_ms(5);
        note(&l, &s, "c.end");
    })?;
    join_all(&[a, b, c], "fifo threads")?;
    let v = log.lock().clone();
    let at = |w: &str| v.iter().find(|e| e.0 == w).map(|e| e.1).ok_or(format!("{w} missing in {v:?}"));
    let (a0, a1, b0, c0, c1) = (at("a.start")?, at("a.end")?, at("b.start")?, at("c.start")?, at("c.end")?);
    kassert!(c0 > a0 && c0 < a1, "the higher priority did not preempt A: {v:?}");
    kassert!(c1 < a1, "A finished before the higher priority thread: {v:?}");
    kassert!(b0 > a1, "B (same priority, queued later) started before A ended: {v:?}");
    Ok(())
}

/// Interactive, Normal and Background busy on one CPU: CPU time in that order (weights 3121 / 1024 / 110), and
/// none starves.
fn fair_weights() -> R {
    let Some((t, _pin)) = test_cpu() else { return ktest::skip("needs two CPUs") };
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    for (name, class) in [("sched-w-int", Class::Interactive), ("sched-w-norm", Class::Normal), ("sched-w-bg", Class::Background)] {
        let s = stop.clone();
        hs.push(spawn_on(name, class, Some(t), true, 0, move || {
            while !s.load(Ordering::Acquire) {
                spin(1_000);
                cond_resched();
            }
        })?);
    }
    crate::sleep_ms(50);
    let r0: Vec<u64> = hs.iter().map(|h| h.thread().runtime_ns()).collect();
    crate::sleep_ms(1_000);
    let r1: Vec<u64> = hs.iter().map(|h| h.thread().runtime_ns()).collect();
    stop.store(true, Ordering::Release);
    let j = join_all(&hs, "weights");
    j?;
    let d: Vec<u64> = r0.iter().zip(r1.iter()).map(|(a, b)| b - a).collect();
    klog::event!(Info, "sched", "ktest.weights", interactive_ms = d[0] / MS, normal_ms = d[1] / MS, background_ms = d[2] / MS);
    kassert!(d[0] > d[1] && d[1] > d[2], "CPU time interactive {} ms, normal {} ms, background {} ms: not in weight order", d[0] / MS, d[1] / MS, d[2] / MS);
    kassert!(d[2] > 0, "the Background thread starved");
    Ok(())
}

/// An Interactive thread that sleeps and wakes on a CPU busy with two Normal threads runs soon after each wake-up
/// (latency bonus); the latencies are reported, judged only on hardware.
fn interactive_wake() -> R {
    let Some((t, _pin)) = test_cpu() else { return ktest::skip("needs two CPUs") };
    let stop = Arc::new(AtomicBool::new(false));
    let mut hogs = Vec::new();
    for _ in 0..2 {
        let s = stop.clone();
        hogs.push(spawn_on("sched-hog", Class::Normal, Some(t), true, 0, move || {
            while !s.load(Ordering::Acquire) {
                spin(1_000);
                cond_resched();
            }
        })?);
    }
    let lat = Arc::new(SpinLock::new(Vec::new()));
    let l = lat.clone();
    let it = spawn_on("sched-inter", Class::Interactive, Some(t), true, 0, move || {
        for _ in 0..20 {
            let want = time::now_ns() + 5 * MS;
            crate::sleep_ns(5 * MS);
            l.lock().push(time::now_ns().saturating_sub(want));
        }
    })?;
    let r = join_all(core::slice::from_ref(&it), "interactive");
    stop.store(true, Ordering::Release);
    let j = join_all(&hogs, "hogs");
    r?;
    j?;
    let v = lat.lock();
    kassert!(v.len() == 20, "{} of 20 wake-ups", v.len());
    let max = v.iter().copied().max().unwrap_or(0);
    klog::event!(Info, "sched", "ktest.interactive_wake", max_us = max / 1000, avg_us = v.iter().sum::<u64>() / v.len().max(1) as u64 / 1000);
    if !hypervisor() {
        kassert!(max < 20 * MS, "an Interactive wake-up took {} ms under two busy threads", max / MS);
    }
    Ok(())
}

static IRQ_WQ: WaitQueue = WaitQueue::new();
static IRQ_TOKENS: AtomicU64 = AtomicU64::new(0);
static IRQ_IN_HANDLER: AtomicU64 = AtomicU64::new(0);
static IRQ_DONE: Completion = Completion::new();

fn wake_handler(_v: u8) {
    if hal::percpu::in_hard_irq() {
        IRQ_IN_HANDLER.fetch_add(1, Ordering::Relaxed);
    }
    IRQ_TOKENS.fetch_add(1, Ordering::AcqRel);
    IRQ_WQ.wake_one();
    IRQ_DONE.complete();
}

/// Wake-ups from a hard-interrupt handler (a self-IPI on a vector of this CPU): a waiter on this CPU and one on
/// another CPU each wake for every interrupt, `Completion::complete` from the handler too; no lock-order finding.
fn wake_irq() -> R {
    if !acpi::irq::enabled() {
        return ktest::skip("interrupts are not on");
    }
    let n = crate::wait_cpus_online(5 * SEC);
    let me = current_cpu();
    let _pin = crate::pin_for_now(me).ok_or("could not pin the driver")?;
    let v = hal::vectors::alloc(wake_handler, Some(me as u32)).map_err(|e| format!("vector: {e:?}"))?;
    let unexpected0 = ksync::lockdep::counts().unexpected;
    let irq_wakes0: u64 = crate::cpu_stats().iter().map(|s| s.irq_wakes).sum();
    let result = (|| -> R {
        let other = (0..crate::cpus::count()).find(|&c| c != me && crate::cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire)));
        for (label, cpu) in [("same CPU", Some(me)), ("other CPU", other)] {
            let Some(cpu) = cpu else { continue };
            const ROUNDS: u64 = 100;
            let consumed = Arc::new(AtomicU64::new(IRQ_TOKENS.load(Ordering::Acquire)));
            let woke = Arc::new(AtomicU64::new(0));
            let (c, w) = (consumed.clone(), woke.clone());
            let waiter = spawn_on("sched-irqwait", Class::Normal, Some(cpu), true, 0, move || {
                for _ in 0..ROUNDS {
                    let want = c.load(Ordering::Acquire) + 1;
                    if IRQ_WQ.wait_until_timeout(|| IRQ_TOKENS.load(Ordering::Acquire) >= want, 5 * SEC) {
                        c.store(want, Ordering::Release);
                        w.fetch_add(1, Ordering::AcqRel);
                    } else {
                        return;
                    }
                }
            })?;
            for r in 0..ROUNDS {
                // Wait until the waiter sleeps, so the interrupt really has to wake it.
                let t0 = time::now_ns();
                while IRQ_WQ.waiter_count() == 0 && woke.load(Ordering::Acquire) == r && time::now_ns() - t0 < SEC {
                    crate::yield_now();
                    hal::cpu::spin_hint();
                }
                hal::apic::send_ipi(hal::apic::IpiDest::SelfOnly, hal::apic::IpiKind::Fixed(v.vector)).map_err(|e| format!("self-IPI: {e:?}"))?;
                let t0 = time::now_ns();
                while woke.load(Ordering::Acquire) <= r && time::now_ns() - t0 < 5 * SEC {
                    crate::sleep_ms(1);
                }
                kassert!(woke.load(Ordering::Acquire) > r, "{label}: interrupt {r} did not wake the waiter within 5 s");
            }
            join_all(core::slice::from_ref(&waiter), "irq waiter")?;
        }
        // A completion from the handler wakes a thread waiting on it.
        IRQ_DONE.reset();
        let done = Arc::new(AtomicBool::new(false));
        let d = done.clone();
        let w = spawn_on("sched-irqdone", Class::Interactive, other, other.is_some(), 0, move || {
            d.store(IRQ_DONE.wait_timeout(5 * SEC), Ordering::Release);
        })?;
        crate::sleep_ms(5);
        hal::apic::send_ipi(hal::apic::IpiDest::SelfOnly, hal::apic::IpiKind::Fixed(v.vector)).map_err(|e| format!("self-IPI: {e:?}"))?;
        join_all(core::slice::from_ref(&w), "completion waiter")?;
        kassert!(done.load(Ordering::Acquire), "Completion::complete from the handler did not wake the waiter");
        kassert!(IRQ_IN_HANDLER.load(Ordering::Relaxed) > 0, "the handler never saw hard-IRQ context");
        Ok(())
    })();
    hal::vectors::free(v);
    result?;
    crate::report_lock_violations();
    let unexpected = ksync::lockdep::counts().unexpected - unexpected0;
    kassert!(unexpected == 0, "{unexpected} lock-order findings during interrupt-time wake-ups: {:?}", ksync::lockdep::list());
    let irq_wakes: u64 = crate::cpu_stats().iter().map(|s| s.irq_wakes).sum::<u64>() - irq_wakes0;
    kassert!(irq_wakes > 0, "no wake-up went through the interrupt-time path");
    klog::event!(Info, "sched", "ktest.wake_irq", cpus = n, irq_wakes = irq_wakes);
    Ok(())
}

/// Threads all started on one CPU end up spread over the CPUs (stealing and pushing).
fn balance_spread() -> R {
    let ncpu = crate::wait_cpus_online(5 * SEC);
    if ncpu < 2 {
        return ktest::skip("needs two CPUs");
    }
    let n = (2 * ncpu).min(16);
    let start = current_cpu();
    let moved0: u64 = crate::cpu_stats().iter().map(|s| s.steals + s.pushes).sum();
    let stop = Arc::new(AtomicBool::new(false));
    let seen = Arc::new(SpinLock::new(BTreeSet::new()));
    let mut hs = Vec::new();
    for _ in 0..n {
        let (s, seen) = (stop.clone(), seen.clone());
        hs.push(spawn_on("sched-spread", Class::Normal, Some(start), false, 0, move || {
            let mut last = usize::MAX;
            while !s.load(Ordering::Acquire) {
                let c = current_cpu();
                if c != last {
                    seen.lock().insert(c);
                    last = c;
                }
                spin(2_000);
                cond_resched();
            }
        })?);
    }
    let want = ncpu.min(n);
    let t0 = time::now_ns();
    while seen.lock().len() < want && time::now_ns() - t0 < 10 * SEC {
        crate::sleep_ms(5);
    }
    stop.store(true, Ordering::Release);
    let j = join_all(&hs, "spread");
    let migr: u64 = hs.iter().map(|h| h.thread().migrations()).sum();
    drop(hs);
    j?;
    let used = seen.lock().len();
    let moved: u64 = crate::cpu_stats().iter().map(|s| s.steals + s.pushes).sum::<u64>() - moved0;
    klog::event!(Info, "sched", "ktest.spread", threads = n, cpus = ncpu, used = used, moved = moved, migrations = migr);
    kassert!(used >= want, "{n} threads started on CPU {start} ran on only {used} of {ncpu} CPUs");
    kassert!(moved > 0 && migr > 0, "no steal or push moved a thread");
    Ok(())
}

/// The reaper's sync generations: a new generation is passed by every CPU only after it flushed (the IPI nudges
/// halted CPUs), and threads that ran on several CPUs and exited give their stacks and records back.
fn reap_sync() -> R {
    let ncpu = crate::wait_cpus_online(5 * SEC);
    let g = hal::switch::request_sync();
    let t0 = time::now_ns();
    while !hal::switch::sync_passed(g) && time::now_ns() - t0 < 5 * SEC {
        for c in hal::switch::sync_lagging(g).collect::<Vec<_>>() {
            if c == current_cpu() {
                hal::switch::quiescent_point();
            } else {
                crate::cpus::send_resched(c);
            }
        }
        crate::sleep_ms(1);
    }
    kassert!(hal::switch::sync_passed(g), "CPUs {:?} did not pass generation {g} in 5 s", hal::switch::sync_lagging(g).collect::<Vec<_>>());
    let live0 = crate::stack_stats().live;
    let threads0 = crate::thread_count();
    let mut hs = Vec::new();
    for k in 0..(2 * ncpu).min(32) {
        hs.push(spawn_on("sched-wander", Class::Normal, None, false, 0, move || {
            for step in 0..4 {
                let c = (k + step) % ncpu;
                crate::pin_current(Some(c));
                spin(1_000);
            }
        })?);
    }
    join_all(&hs, "wanderers")?;
    let wandered: u64 = hs.iter().map(|h| h.thread().migrations()).sum();
    drop(hs);
    kassert!(crate::reap(), "the reaper did not free everything within 2 s (stacks retired: {})", hal::switch::retired_count());
    kassert!(crate::stack_stats().live <= live0, "{} stacks live, {live0} before", crate::stack_stats().live);
    kassert!(crate::thread_count() <= threads0, "{} threads listed, {threads0} before", crate::thread_count());
    kassert!(ncpu < 2 || wandered > 0, "the threads never moved");
    Ok(())
}

/// The interrupt-exit hook: it never preempts with a spin lock held or when the interrupted code had interrupts
/// off; with a reschedule pending it switches to the waiting KernelRt thread and returns once this thread runs
/// again.
fn preempt_hook() -> R {
    let me = current_cpu();
    let _pin = crate::pin_for_now(me).ok_or("could not pin")?;
    let ran = Arc::new(AtomicBool::new(false));
    let r = ran.clone();
    // KernelRt: it deserves the CPU over the (fair) caller whatever the virtual runtimes say.
    let x = spawn_on("sched-preempted", Class::KernelRt, Some(me), true, crate::RT_PRIO_DEFAULT, move || r.store(true, Ordering::Release))?;
    let cs = crate::cpus::of(me).ok_or("no CPU state")?;
    let result = (|| -> R {
        let lock = SpinLock::new(());
        {
            let _g = lock.lock();
            kassert!(ksync::preempt_count() > 0, "a held spin lock does not count");
            cs.need_resched.store(true, Ordering::Release);
            let _irq = hal::cpu::IrqGuard::new();
            kassert!(!crate::irq_exit_preempt(true), "preempted a spin-lock holder");
        }
        cs.need_resched.store(true, Ordering::Release);
        {
            let _irq = hal::cpu::IrqGuard::new();
            kassert!(!crate::irq_exit_preempt(false), "preempted code that had interrupts off");
        }
        cs.need_resched.store(true, Ordering::Release);
        let switched = {
            let _irq = hal::cpu::IrqGuard::new();
            crate::irq_exit_preempt(true)
        };
        kassert!(switched, "a pending reschedule did not preempt");
        let t0 = time::now_ns();
        while !ran.load(Ordering::Acquire) && time::now_ns() - t0 < 5 * SEC {
            crate::sleep_ms(1);
        }
        kassert!(ran.load(Ordering::Acquire), "the waiting thread did not run");
        Ok(())
    })();
    cs.need_resched.store(false, Ordering::Release);
    let j = join_all(core::slice::from_ref(&x), "preempted");
    result?;
    j
}

/// `run_exclusive`: busy threads on the other CPUs make no progress while the section runs, and go on after it.
fn exclusive() -> R {
    let ncpu = crate::wait_cpus_online(5 * SEC);
    if ncpu < 2 {
        return ktest::skip("needs two CPUs");
    }
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for _ in 0..ncpu {
        let (s, c) = (stop.clone(), count.clone());
        hs.push(spawn_on("sched-excl", Class::Normal, None, false, 0, move || {
            while !s.load(Ordering::Acquire) {
                c.fetch_add(1, Ordering::Relaxed);
                spin(500);
                cond_resched();
            }
        })?);
    }
    crate::sleep_ms(20);
    let r = crate::run_exclusive(5 * SEC, || {
        let me = current_cpu();
        // Threads queued on this CPU do not run either: this one does not give the CPU up.
        let a = count.load(Ordering::Acquire);
        let t0 = time::now_ns();
        while time::now_ns() - t0 < 20 * MS {
            spin(500);
        }
        (me, count.load(Ordering::Acquire) - a)
    });
    let after = count.load(Ordering::Acquire);
    crate::sleep_ms(20);
    let moved_on = count.load(Ordering::Acquire) > after;
    stop.store(true, Ordering::Release);
    let j = join_all(&hs, "exclusive");
    j?;
    let (cpu, delta) = r.map_err(|e| format!("{e:?}"))?;
    kassert!(delta == 0, "other threads advanced {delta} times during the exclusive section on CPU {cpu}");
    kassert!(moved_on, "the threads did not go on after the section");
    Ok(())
}

static LAT_WQ: WaitQueue = WaitQueue::new();
static LAT_STAMP: AtomicU64 = AtomicU64::new(0);
static LAT_SEQ: AtomicU64 = AtomicU64::new(0);

fn lat_handler(_v: u8) {
    LAT_STAMP.store(time::now_ns(), Ordering::Release);
    LAT_SEQ.fetch_add(1, Ordering::AcqRel);
    LAT_WQ.wake_one();
}

/// The console-echo path in small (spec §9.2: echo < 20 ms under 32 busy threads): a KernelRt thread woken by an
/// interrupt while 32 Normal threads keep every CPU busy runs soon after each interrupt. Latencies are reported;
/// the 20 ms bound is judged on hardware only.
fn rt_latency() -> R {
    if !acpi::irq::enabled() {
        return ktest::skip("interrupts are not on");
    }
    const BUSY: usize = 32;
    const ROUNDS: u64 = 100;
    let _ = crate::wait_cpus_online(5 * SEC);
    let me = current_cpu();
    let _pin = crate::pin_for_now(me).ok_or("could not pin the driver")?;
    let v = hal::vectors::alloc(lat_handler, Some(me as u32)).map_err(|e| format!("vector: {e:?}"))?;
    let stop = Arc::new(AtomicBool::new(false));
    let mut hs = Vec::new();
    let result = (|| -> R {
        for _ in 0..BUSY {
            let s = stop.clone();
            hs.push(spawn_on("sched-load", Class::Normal, None, false, 0, move || {
                while !s.load(Ordering::Acquire) {
                    spin(2_000);
                    cond_resched();
                }
            })?);
        }
        let base = LAT_SEQ.load(Ordering::Acquire);
        let lat = Arc::new(SpinLock::new(Vec::new()));
        let l = lat.clone();
        let echo = spawn_on("sched-echo", Class::KernelRt, None, false, crate::RT_PRIO_DEFAULT, move || {
            for i in 1..=ROUNDS {
                if !LAT_WQ.wait_until_timeout(|| LAT_SEQ.load(Ordering::Acquire) >= base + i, 5 * SEC) {
                    return;
                }
                l.lock().push(time::now_ns().saturating_sub(LAT_STAMP.load(Ordering::Acquire)));
            }
        })?;
        crate::sleep_ms(20);
        for i in 0..ROUNDS {
            let t0 = time::now_ns();
            while LAT_WQ.waiter_count() == 0 && time::now_ns() - t0 < SEC {
                crate::sleep_ms(1);
            }
            hal::apic::send_ipi(hal::apic::IpiDest::SelfOnly, hal::apic::IpiKind::Fixed(v.vector)).map_err(|e| format!("self-IPI: {e:?}"))?;
            let t0 = time::now_ns();
            while lat.lock().len() as u64 <= i && time::now_ns() - t0 < 5 * SEC {
                crate::sleep_ms(1);
            }
            kassert!(lat.lock().len() as u64 > i, "interrupt {i}: the KernelRt thread did not run within 5 s");
        }
        join_all(core::slice::from_ref(&echo), "echo")?;
        let v = lat.lock().clone();
        let max = v.iter().copied().max().unwrap_or(0);
        let avg = v.iter().sum::<u64>() / v.len().max(1) as u64;
        klog::event!(Info, "sched", "ktest.rt_latency", busy = BUSY, cpus = crate::online_cpus(), max_us = max / 1000, avg_us = avg / 1000);
        if !hypervisor() {
            kassert!(max < 20 * MS, "a KernelRt wake-up took {} ms under {BUSY} busy threads", max / MS);
        }
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    let j = join_all(&hs, "load");
    hal::vectors::free(v);
    result?;
    j
}
