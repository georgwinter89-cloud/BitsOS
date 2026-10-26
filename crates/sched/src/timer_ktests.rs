//! SC-4 ktests: sleeps end through the sleep timer and are never short, sleeping threads cost (almost) nothing,
//! timed waits end at their deadline (and earlier when woken), the tick is armed only with more than one
//! runnable thread, every CPU's heartbeat moves, and idle entries are recorded with IF=1. Timing is never judged
//! under QEMU (generous functional limits there); the Mini-PC numbers come from the `sched ktest.*` events.

use crate::timers::{self, FALLBACK_EXPIRES, SLEEP_FIRED, TICK_KICKS, TICKS_ARMED, TICKS_FIRED};
use crate::{Class, JoinHandle, SpawnOpts, cond_resched, current_cpu, spawn_with};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use ksync::{Completion, SpinLock, WaitQueue};
use ktest::{kassert, ktest};

type R = ktest::TestResult;

ktest!("sched.timer.sleep_accuracy", Safe, sleep_accuracy);
ktest!("sched.timer.sleepers_cost", Safe, sleepers_cost);
ktest!("sched.timer.wait_timeout", Safe, wait_timeout);
ktest!("sched.timer.wait_timeout_many", Safe, wait_timeout_many);
ktest!("sched.tick.only_when_busy", Safe, tick_only_when_busy);
ktest!("sched.tick.remote_enqueue", Safe, tick_remote_enqueue);
ktest!("sched.tick.slices_spinners", Safe, tick_slices_spinners);
ktest!("sched.heartbeat.all_cpus", Safe, heartbeat_all_cpus);
ktest!("sched.heartbeat.probe_halted", Safe, heartbeat_probe_halted);
ktest!("sched.idle.record_if", Safe, idle_record);
ktest!("sched.timer.sleep_beside_stuck_cpu", QemuOnly, sleep_beside_stuck_cpu);

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
/// QEMU (TCG or a loaded host) can stall a vCPU for long stretches: a functional bound only.
const QEMU_SLACK: u64 = 500 * MS;
/// Hardware: the poll-duty CPU fires hrtimers within microseconds, so on average a sleep or a timed wait ends
/// well within a millisecond; single ones may wait for a busy kernel thread on their CPU to reach a preemption
/// point (until CP-5), bounded by the console-echo target.
const HW_AVG_SLACK: u64 = MS;
const HW_MAX_SLACK: u64 = 20 * MS;

fn hypervisor() -> bool {
    hal::cpu::live().has("hypervisor")
}

fn slack() -> u64 {
    if hypervisor() { QEMU_SLACK } else { HW_MAX_SLACK }
}

fn check_lateness(what: &str, max: u64, avg: u64) -> R {
    kassert!(max < slack(), "a {what} ended {} us late", max / 1000);
    if !hypervisor() {
        kassert!(avg < HW_AVG_SLACK, "{what}s ended {} us late on average", avg / 1000);
    }
    Ok(())
}

fn spawn_on(name: &str, class: Class, cpu: Option<usize>, pin: bool, f: impl FnOnce() + Send + 'static) -> Result<JoinHandle, String> {
    spawn_with(name, class, SpawnOpts { cpu, pin, rt_prio: crate::RT_PRIO_DEFAULT }, f).map_err(|e| format!("spawn {name}: {e:?}"))
}

fn join_all(hs: &[JoinHandle], what: &str) -> R {
    for h in hs {
        kassert!(h.join_timeout(30 * SEC), "{what}: thread {} did not finish within 30 s", h.id());
    }
    Ok(())
}

fn first_error(errs: &SpinLock<Vec<String>>) -> R {
    match errs.lock().first() {
        Some(e) => Err(e.clone()),
        None => Ok(()),
    }
}

fn online() -> Vec<usize> {
    (0..crate::cpus::count()).filter(|&c| crate::cpus::of(c).is_some_and(|s| s.online.load(Ordering::Acquire))).collect()
}

/// Sleeps of 1..20 ms are never short and end soon after their deadline; the sleep timer (or, with every CPU
/// busy, the preemption-point fallback) ends them.
fn sleep_accuracy() -> R {
    kassert!(timers::ready(), "timers not set up");
    let (fired0, fb0) = (SLEEP_FIRED.get(), FALLBACK_EXPIRES.get());
    let mut over = Vec::new();
    for ms in [1u64, 2, 5, 10, 20] {
        for _ in 0..5 {
            let t0 = time::now_ns();
            crate::sleep_ms(ms);
            let d = time::now_ns() - t0;
            kassert!(d >= ms * MS, "sleep_ms({ms}) returned after {d} ns");
            over.push(d - ms * MS);
        }
    }
    let max = over.iter().copied().max().unwrap_or(0);
    let avg = over.iter().sum::<u64>() / over.len() as u64;
    let (fired, fb) = (SLEEP_FIRED.get() - fired0, FALLBACK_EXPIRES.get() - fb0);
    klog::event!(Info, "sched", "ktest.sleep", sleeps = over.len(), max_over_us = max / 1000, avg_over_us = avg / 1000, timer_fired = fired, fallback = fb);
    kassert!(fired + fb > 0, "no sleep ended through the sleep timer or the fallback");
    check_lateness("sleep", max, avg)
}

/// 100 threads sleeping 10 ms ten times each: every sleep at least 10 ms, all done, and the sleepers together
/// use only a sliver of the CPUs (spec §9.2: sleeping threads cost nothing; < 5 % asserted on hardware).
fn sleepers_cost() -> R {
    const N: usize = 100;
    const ROUNDS: u64 = 10;
    let errs = Arc::new(SpinLock::new(Vec::new()));
    let t0 = time::now_ns();
    let mut hs = Vec::new();
    for _ in 0..N {
        let e = errs.clone();
        hs.push(spawn_on("sched-sleeper", Class::Normal, None, false, move || {
            for _ in 0..ROUNDS {
                let a = time::now_ns();
                crate::sleep_ms(10);
                let d = time::now_ns() - a;
                if d < 10 * MS {
                    e.lock().push(format!("a 10 ms sleep returned after {d} ns"));
                }
            }
        })?);
    }
    join_all(&hs, "sleepers")?;
    let wall = time::now_ns() - t0;
    let used: u64 = hs.iter().map(|h| h.thread().runtime_ns()).sum();
    let cpus = online().len().max(1) as u64;
    let pct_x100 = used * 10_000 / (wall * cpus).max(1);
    klog::event!(Info, "sched", "ktest.sleepers_cost", threads = N, wall_ms = wall / MS, runtime_us = used / 1000, cpus = cpus,
        cost_pct_x100 = pct_x100);
    first_error(&errs)?;
    if !hypervisor() {
        kassert!(pct_x100 < 500, "{N} sleeping threads used {}.{:02} % of the CPUs", pct_x100 / 100, pct_x100 % 100);
    }
    crate::reap();
    Ok(())
}

/// A timed wait whose condition never comes true ends at its deadline (not before, soon after); one that is
/// woken ends at once; `Completion::wait_timeout` and `JoinHandle::join_timeout` likewise.
fn wait_timeout() -> R {
    static WQ: WaitQueue = WaitQueue::new();
    // Events only after the measurements: a log line makes klogd draw on the console, and until CP-5 a woken
    // thread waits for a busy kernel thread on its CPU to reach a preemption point.
    let mut over = Vec::new();
    for ms in [1u64, 2, 5, 10] {
        for _ in 0..3 {
            let t0 = time::now_ns();
            let ok = WQ.wait_until_timeout(|| false, ms * MS);
            let d = time::now_ns() - t0;
            kassert!(!ok, "a never-true condition reported true");
            kassert!(d >= ms * MS, "{ms} ms timeout ended after {d} ns");
            over.push(d - ms * MS);
        }
    }
    let (max, avg) = (over.iter().copied().max().unwrap_or(0), over.iter().sum::<u64>() / over.len() as u64);
    klog::event!(Info, "sched", "ktest.wait_timeout", waits = over.len(), max_over_us = max / 1000, avg_over_us = avg / 1000);
    check_lateness("timed wait", max, avg)?;
    // Woken before the deadline: true, long before the 5 s timeout.
    let flag = Arc::new(AtomicBool::new(false));
    let f = flag.clone();
    let h = spawn_on("sched-waker", Class::Normal, None, false, move || {
        crate::sleep_ms(5);
        f.store(true, Ordering::Release);
        WQ.wake_all();
    })?;
    let t0 = time::now_ns();
    let ok = WQ.wait_until_timeout(|| flag.load(Ordering::Acquire), 5 * SEC);
    let d = time::now_ns() - t0;
    join_all(core::slice::from_ref(&h), "waker")?;
    kassert!(ok, "the woken wait timed out");
    kassert!(d < 5 * SEC, "the woken wait took {d} ns");
    // Completion and join.
    let c = Completion::new();
    let t0 = time::now_ns();
    kassert!(!c.wait_timeout(3 * MS), "an incomplete completion reported done");
    kassert!(time::now_ns() - t0 >= 3 * MS, "Completion::wait_timeout was short");
    let h = spawn_on("sched-long", Class::Normal, None, false, || crate::sleep_ms(200))?;
    let t0 = time::now_ns();
    kassert!(!h.join_timeout(5 * MS), "join_timeout reported a sleeping thread finished");
    kassert!(time::now_ns() - t0 >= 5 * MS, "join_timeout was short");
    kassert!(h.join_timeout(30 * SEC), "the sleeping thread did not finish");
    crate::reap();
    Ok(())
}

/// 16 threads wait on one queue with timeouts of 2, 4, .. 32 ms and nobody wakes them: each ends at or after
/// its own deadline (the sleep timer re-arms for the next one), none is lost. The order they end in is reported
/// (not asserted: until CP-5 a woken waiter may wait for a busy kernel thread on its CPU).
fn wait_timeout_many() -> R {
    static WQ: WaitQueue = WaitQueue::new();
    const N: u64 = 16;
    let ends = Arc::new(SpinLock::new(Vec::new()));
    let errs = Arc::new(SpinLock::new(Vec::new()));
    let start = time::now_ns() + 20 * MS;
    let mut hs = Vec::new();
    for i in 1..=N {
        let (en, e) = (ends.clone(), errs.clone());
        hs.push(spawn_on("sched-timed", Class::Normal, None, false, move || {
            // All start from the same moment, so the deadlines are 2 ms apart.
            while time::now_ns() < start {
                crate::sleep_ns(start - time::now_ns());
            }
            let t = 2 * MS * i;
            let ok = WQ.wait_until_timeout(|| false, t);
            let end = time::now_ns();
            let d = end - start;
            if ok {
                e.lock().push(format!("waiter {i}: never-true condition reported true"));
            } else if d < t {
                e.lock().push(format!("waiter {i}: {t} ns timeout ended after {d} ns"));
            } else if d > t + slack() {
                e.lock().push(format!("waiter {i}: ended {} us late", (d - t) / 1000));
            }
            en.lock().push((i, end));
        })?);
    }
    join_all(&hs, "timed waiters")?;
    first_error(&errs)?;
    let mut v = ends.lock().clone();
    kassert!(v.len() as u64 == N, "{} of {N} waiters ended", v.len());
    v.sort_by_key(|e| e.1);
    let order: Vec<u64> = v.iter().map(|e| e.0).collect();
    let in_order = order.windows(2).all(|w| w[0] < w[1]);
    klog::event!(Info, "sched", "ktest.wait_timeout_many", waiters = N, in_order = in_order, order = format!("{order:?}"));
    kassert!(WQ.waiter_count() == 0, "{} waiters left on the queue", WQ.waiter_count());
    crate::reap();
    Ok(())
}

/// Two busy fair threads pinned to one CPU arm its tick and the tick asks for switches; one alone, or none,
/// leaves the CPU tickless (an armed tick fires once more and stops).
fn tick_only_when_busy() -> R {
    let n = crate::wait_cpus_online(5 * SEC);
    if n < 2 {
        return ktest::skip("needs two CPUs (the tick of a busy CPU is fired by the polling one until CP-5)");
    }
    let target = *online().last().ok_or("no CPU")?;
    let home = *online().iter().find(|&&c| c != target).ok_or("no second CPU")?;
    let _pin = crate::pin_for_now(home).ok_or("could not pin the driver")?;
    let stops: Vec<Arc<AtomicBool>> = (0..2).map(|_| Arc::new(AtomicBool::new(false))).collect();
    let mut hs = Vec::new();
    for s in &stops {
        let s = s.clone();
        hs.push(spawn_on("sched-ticked", Class::Normal, Some(target), true, move || {
            while !s.load(Ordering::Acquire) {
                let mut x = 0u64;
                for i in 0..2_000u64 {
                    x = core::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
                }
                core::hint::black_box(x);
                cond_resched();
            }
        })?);
    }
    let result = (|| -> R {
        let (k0, f0) = (TICK_KICKS.get(), TICKS_FIRED.get());
        // Two runnable: the tick is armed and kicks (functional: within 5 s).
        let t0 = time::now_ns();
        let mut armed_seen = false;
        while (TICK_KICKS.get() == k0 || !armed_seen) && time::now_ns() - t0 < 5 * SEC {
            armed_seen |= timers::tick_armed(target);
            crate::sleep_ms(1);
        }
        kassert!(armed_seen, "CPU {target} with two busy threads never had its tick armed");
        kassert!(TICK_KICKS.get() > k0, "the tick of CPU {target} never asked for a switch (fired {})", TICKS_FIRED.get() - f0);
        // One runnable: the tick stops (after at most one more firing; other threads may visit, so "a moment
        // without a tick" within 5 s).
        stops[0].store(true, Ordering::Release);
        join_all(core::slice::from_ref(&hs[0]), "first busy thread")?;
        let t0 = time::now_ns();
        let mut stopped = false;
        while !stopped && time::now_ns() - t0 < 5 * SEC {
            crate::sleep_ms(2);
            stopped = !timers::tick_armed(target);
        }
        kassert!(stopped, "CPU {target} with one busy thread kept its tick armed for 5 s");
        klog::event!(Info, "sched", "ktest.tick", cpu = target, kicks = TICK_KICKS.get() - k0, fired = TICKS_FIRED.get() - f0);
        Ok(())
    })();
    for s in &stops {
        s.store(true, Ordering::Release);
    }
    let j = join_all(&hs, "busy threads");
    result?;
    j?;
    crate::reap();
    Ok(())
}

/// Every online CPU's heartbeat moves: by a thread pinned there (switches) and, for a CPU halted with nothing to
/// do, by `probe_cpu` alone (the reschedule IPI's handler).
fn heartbeat_all_cpus() -> R {
    let _ = crate::wait_cpus_online(5 * SEC);
    let cpus = online();
    let before: Vec<u64> = cpus.iter().map(|&c| crate::heartbeat(c).unwrap_or(0)).collect();
    // A probe first: a halted CPU answers it without any thread of ours.
    let me = current_cpu();
    let mut probed = Vec::new();
    for &c in &cpus {
        if c != me && crate::cpus::of(c).is_some_and(|s| s.halted.load(Ordering::Acquire)) {
            let hb = crate::heartbeat(c).unwrap_or(0);
            if crate::probe_cpu(c) {
                probed.push((c, hb));
            }
        }
    }
    let t0 = time::now_ns();
    for &(c, hb) in &probed {
        while crate::heartbeat(c).unwrap_or(0) == hb && time::now_ns() - t0 < 2 * SEC {
            crate::sleep_ms(1);
        }
        kassert!(crate::heartbeat(c).unwrap_or(0) != hb, "CPU {c} did not answer a probe within 2 s");
    }
    kassert!(!crate::probe_cpu(me), "probing the own CPU must be refused");
    let ran = Arc::new(AtomicUsize::new(0));
    let mut hs = Vec::new();
    for &c in &cpus {
        let r = ran.clone();
        hs.push(spawn_on("sched-beat", Class::Normal, Some(c), true, move || {
            crate::sleep_ms(1);
            r.fetch_add(1, Ordering::AcqRel);
        })?);
    }
    join_all(&hs, "pinned threads")?;
    kassert!(ran.load(Ordering::Acquire) == cpus.len(), "pinned threads ran {} of {}", ran.load(Ordering::Acquire), cpus.len());
    let after: Vec<u64> = cpus.iter().map(|&c| crate::heartbeat(c).unwrap_or(0)).collect();
    let still: Vec<usize> = cpus.iter().zip(before.iter().zip(&after)).filter(|(_, (b, a))| a == b).map(|(c, _)| *c).collect();
    klog::event!(Info, "sched", "ktest.heartbeat", cpus = cpus.len(), probed = probed.len(), before = format!("{before:?}"), after = format!("{after:?}"));
    kassert!(still.is_empty(), "heartbeat did not move on CPUs {still:?}");
    kassert!(crate::heartbeat(usize::MAX).is_none(), "a heartbeat for a CPU that does not exist");
    crate::reap();
    Ok(())
}

/// Every online CPU, made to run a thread once, goes idle again in a quiet moment and records it with IF=1 (also
/// a halted one, if there is one); the record's time lies in the test's window.
fn idle_record() -> R {
    let _ = crate::wait_cpus_online(5 * SEC);
    let cpus = online();
    let t0 = time::now_ns();
    // A thread on every CPU makes each leave idle (the poll-duty CPU would otherwise stay in one idle period).
    let mut hs = Vec::new();
    for &c in &cpus {
        hs.push(spawn_on("sched-unidle", Class::Normal, Some(c), true, || {})?);
    }
    join_all(&hs, "pinned threads")?;
    crate::reap();
    let mut missing = cpus.clone();
    let mut halted_seen = None;
    while !missing.is_empty() && time::now_ns() - t0 < 5 * SEC {
        // Sleeping lets this CPU go idle too.
        crate::sleep_ms(2);
        let now = time::now_ns();
        let mut left = Vec::new();
        for &c in &missing {
            match crate::last_idle_entry(c) {
                Some(e) if e.at_ns >= t0 => {
                    kassert!(e.at_ns <= now, "CPU {c}: idle entry {} ns in the future", e.at_ns - now);
                    kassert!(e.interrupts_on, "CPU {c} went idle with interrupts off at {} ns", e.at_ns);
                    if e.halted_now && e.idle_now {
                        halted_seen = Some(c);
                    }
                }
                _ => left.push(c),
            }
        }
        missing = left;
    }
    kassert!(missing.is_empty(), "CPUs {missing:?} recorded no idle entry within 5 s");
    // A halted CPU, if any is around right now, must say IF=1.
    for &c in &cpus {
        if let Some(e) = crate::last_idle_entry(c)
            && e.halted_now
        {
            kassert!(e.interrupts_on && e.idle_now, "CPU {c} halted with record {e:?}");
            halted_seen = Some(c);
        }
    }
    kassert!(crate::last_idle_entry(usize::MAX).is_none(), "an idle record for a CPU that does not exist");
    klog::event!(Info, "sched", "ktest.idle_record", cpus = cpus.len(), halted_cpu = halted_seen.map_or(-1, |c| c as i64));
    Ok(())
}

/// A thread that spins on its CPU without any preemption point until `stop` or `limit_ns` (B4: bounded),
/// counting its rounds in `count` and saying it started in `started`.
fn spinner(stop: Arc<AtomicBool>, count: Arc<AtomicU64>, limit_ns: u64) -> impl FnOnce() + Send + 'static {
    move || {
        let t0 = time::now_ns();
        while !stop.load(Ordering::Acquire) && time::now_ns() - t0 < limit_ns {
            count.fetch_add(1, Ordering::AcqRel);
            for _ in 0..200 {
                core::hint::spin_loop();
            }
        }
    }
}

/// A busy thread without preemption points, and a second thread placed on its CPU from another one: the
/// placing CPU arms the busy CPU's tick (before SC-4's fix only that CPU's own preemption points did, so the
/// second thread waited without a tick).
fn tick_remote_enqueue() -> R {
    let n = crate::wait_cpus_online(5 * SEC);
    if n < 2 {
        return ktest::skip("needs two CPUs (a busy CPU and one that places work on it)");
    }
    let target = *online().last().ok_or("no CPU")?;
    let home = *online().iter().find(|&&c| c != target).ok_or("no second CPU")?;
    let _pin = crate::pin_for_now(home).ok_or("could not pin the driver")?;
    let stop = Arc::new(AtomicBool::new(false));
    let rounds = Arc::new(AtomicU64::new(0));
    let busy = spawn_on("sched-nopreempt", Class::Normal, Some(target), true, spinner(stop.clone(), rounds.clone(), 10 * SEC))?;
    let result = (|| -> R {
        let t0 = time::now_ns();
        while rounds.load(Ordering::Acquire) == 0 && time::now_ns() - t0 < 5 * SEC {
            crate::sleep_ms(1);
        }
        kassert!(rounds.load(Ordering::Acquire) > 0, "the busy thread never ran on CPU {target}");
        // One runnable there: an old tick fires once more and stops; give it a few slices. (Some other thread
        // placed there meanwhile keeps it armed, rightly: then the check below proves less, and says so.)
        let t0 = time::now_ns();
        while timers::tick_armed(target) && time::now_ns() - t0 < 2 * SEC {
            crate::sleep_ms(2);
        }
        let was_armed = timers::tick_armed(target);
        let armed0 = TICKS_ARMED.get();
        let ran = Arc::new(AtomicBool::new(false));
        let r = ran.clone();
        let second = spawn_on("sched-placed", Class::Normal, Some(target), true, move || r.store(true, Ordering::Release))?;
        // Queued from this CPU: the tick must be armed at once (functional limit: one slice, generous in QEMU).
        let t0 = time::now_ns();
        let limit = if hypervisor() { QEMU_SLACK } else { crate::SLICE_NS };
        let mut armed = timers::tick_armed(target);
        while !armed && time::now_ns() - t0 < limit {
            core::hint::spin_loop();
            armed = timers::tick_armed(target);
        }
        let waited = time::now_ns() - t0;
        stop.store(true, Ordering::Release);
        let joined = second.join_timeout(30 * SEC);
        klog::event!(Info, "sched", "ktest.tick_remote", cpu = target, armed = armed, was_armed = was_armed, waited_us = waited / 1000,
            arms = TICKS_ARMED.get() - armed0, tick_ipis = timers::TICK_IPIS.get());
        kassert!(armed, "CPU {target}: a second thread placed behind a busy one got no tick within {} us", limit / 1000);
        kassert!(joined, "the placed thread did not run within 30 s");
        kassert!(ran.load(Ordering::Acquire), "the placed thread did not run");
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    let j = join_all(core::slice::from_ref(&busy), "busy thread");
    result?;
    j?;
    crate::reap();
    Ok(())
}

/// Two fair threads spinning without preemption points on one CPU both make progress within a few slices: the
/// tick fires on that CPU in interrupt context and the interrupt exit switches. Needs CP-5 (until then the
/// tick only sets the switch request and the first spinner keeps the CPU).
fn tick_slices_spinners() -> R {
    if !timers::tick_preempts() {
        return ktest::skip("needs CP-5: the tick fires on its CPU by interrupt and the interrupt exit preempts");
    }
    let n = crate::wait_cpus_online(5 * SEC);
    if n < 2 {
        return ktest::skip("needs two CPUs (one to spin on, one to watch)");
    }
    let target = *online().last().ok_or("no CPU")?;
    let home = *online().iter().find(|&&c| c != target).ok_or("no second CPU")?;
    let _pin = crate::pin_for_now(home).ok_or("could not pin the driver")?;
    let stop = Arc::new(AtomicBool::new(false));
    let counts: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let mut hs = Vec::new();
    for c in &counts {
        hs.push(spawn_on("sched-spinner", Class::Normal, Some(target), true, spinner(stop.clone(), c.clone(), 10 * SEC))?);
    }
    // About two slices on hardware; a generous functional limit under QEMU.
    let limit = if hypervisor() { 2 * SEC } else { 3 * crate::SLICE_NS };
    let t0 = time::now_ns();
    while counts.iter().any(|c| c.load(Ordering::Acquire) == 0) && time::now_ns() - t0 < limit {
        crate::sleep_ms(1);
    }
    let seen: Vec<u64> = counts.iter().map(|c| c.load(Ordering::Acquire)).collect();
    stop.store(true, Ordering::Release);
    let j = join_all(&hs, "spinners");
    klog::event!(Info, "sched", "ktest.tick_slices", cpu = target, rounds = format!("{seen:?}"), waited_us = (time::now_ns() - t0) / 1000);
    kassert!(seen.iter().all(|&c| c > 0), "spinners on CPU {target} without preemption points: rounds {seen:?} (one never ran)");
    j?;
    crate::reap();
    Ok(())
}

/// A halted CPU (tickless: its heartbeat stands still) answers `probe_cpu`: the heartbeat moves within the
/// watchdog's kind of timeout. RB-3 judges a halted CPU alive only this way, never by its idle record alone.
fn heartbeat_probe_halted() -> R {
    let n = crate::wait_cpus_online(5 * SEC);
    if n < 3 {
        return ktest::skip("needs three CPUs (this one, the poll-duty one, and one that halts)");
    }
    let me = current_cpu();
    let t0 = time::now_ns();
    let mut answered = 0;
    let mut tried = 0;
    // Each candidate once; a CPU that left `hlt` meanwhile does not count.
    while answered == 0 && time::now_ns() - t0 < 5 * SEC {
        crate::sleep_ms(5);
        for c in online().into_iter().filter(|&c| c != me) {
            let Some(e) = crate::last_idle_entry(c) else { continue };
            if !e.halted_now {
                continue;
            }
            let hb = crate::heartbeat(c).unwrap_or(0);
            kassert!(e.interrupts_on && e.idle_now, "CPU {c} halted with record {e:?}");
            kassert!(crate::probe_cpu(c), "probe of halted CPU {c} refused");
            tried += 1;
            let p0 = time::now_ns();
            while crate::heartbeat(c).unwrap_or(0) == hb && time::now_ns() - p0 < 2 * SEC {
                core::hint::spin_loop();
            }
            kassert!(crate::heartbeat(c).unwrap_or(0) != hb, "halted CPU {c} did not answer a probe within 2 s");
            answered += 1;
        }
    }
    klog::event!(Info, "sched", "ktest.probe_halted", cpus = n, probed = tried, answered = answered);
    if answered == 0 {
        return ktest::skip("no CPU halted within 5 s");
    }
    Ok(())
}

static STUCK_STOP: AtomicBool = AtomicBool::new(false);
static STUCK_SPINS: AtomicBool = AtomicBool::new(false);

/// INT-A2a2 regression: a CPU with interrupts off delays only the sleepers that went to sleep there. A thread on
/// the last CPU sleeps first (its deadline is the earliest, its timer there), then that CPU spins with interrupts
/// off for up to 2 s; a thread on CPU 0 sleeping 300 ms meanwhile must end on time. With one sleep timer for the
/// whole system it sat in the stuck CPU's queue and CPU 0's sleep lasted until the spin ended.
fn sleep_beside_stuck_cpu() -> R {
    let n = hal::percpu::cpu_count();
    if n < 2 {
        return ktest::skip("one CPU");
    }
    let k = n - 1;
    let _pin = crate::pin_for_now(0).ok_or("CPU 0 is not online")?;
    let on_k = SpawnOpts { cpu: Some(k), pin: true, ..SpawnOpts::default() };
    let slept = Arc::new(AtomicBool::new(false));
    let s2 = slept.clone();
    let h = spawn_with("t-sleep-on-k", Class::Normal, on_k, move || {
        s2.store(true, Ordering::Release);
        crate::sleep_ns(200 * MS);
    })
    .map_err(|e| format!("spawn sleeper: {e:?}"))?;
    let until = time::now_ns() + 2 * SEC;
    while !(slept.load(Ordering::Acquire) && crate::sleepers::next() != u64::MAX) && time::now_ns() < until {
        core::hint::spin_loop();
    }
    STUCK_STOP.store(false, Ordering::Release);
    STUCK_SPINS.store(false, Ordering::Release);
    let s = spawn_with("t-stuck-k", Class::KernelRt, on_k, || {
        let _irq = hal::cpu::IrqGuard::new();
        STUCK_SPINS.store(true, Ordering::Release);
        let end = time::now_ns() + 2 * SEC;
        while !STUCK_STOP.load(Ordering::Acquire) && time::now_ns() < end {
            // A CPU with interrupts off, not one that ignores TLB shootdowns (as rb.reload's stuck AP).
            hal::paging::tlb::poll();
            core::hint::spin_loop();
        }
        STUCK_SPINS.store(false, Ordering::Release);
    })
    .map_err(|e| format!("spawn spinner: {e:?}"))?;
    // Busy-wait: this test must not sleep before the spin started.
    let until = time::now_ns() + 2 * SEC;
    while !STUCK_SPINS.load(Ordering::Acquire) && time::now_ns() < until {
        core::hint::spin_loop();
    }
    let spinning = STUCK_SPINS.load(Ordering::Acquire);
    let t0 = time::now_ns();
    crate::sleep_ns(300 * MS);
    let took = time::now_ns() - t0;
    let still = STUCK_SPINS.load(Ordering::Acquire);
    STUCK_STOP.store(true, Ordering::Release);
    let joined = s.join_timeout(5 * SEC) && h.join_timeout(5 * SEC);
    kassert!(spinning, "CPU {k} never started its spin");
    kassert!(joined, "the spinner or the sleeper on CPU {k} did not end");
    klog::event!(Info, "sched", "ktest.sleep_beside_stuck", cpu = k, slept_ms = took / MS, spin_still_on = still);
    kassert!(took < SEC, "a 300 ms sleep on CPU 0 took {} ms while CPU {k} had interrupts off", took / MS);
    Ok(())
}
