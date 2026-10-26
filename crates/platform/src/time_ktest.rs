//! CP-5 ktests (all `Safe`: they arm timers, spawn short pinned threads and read counters). Limits under a
//! hypervisor are functional and generous; the tight ones apply on hardware only (spec §11.30: timing is judged
//! on the Mini-PC).

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use hal::percpu::{self, PerCpu};
use ksync::{IrqSpinLock, SpinLock};
use ktest::{kassert, ktest};
use sched::{Class, SpawnOpts};
use time::HrTimer;

type R = ktest::TestResult;

ktest!("cp.time.calibration", Safe, calibration);
ktest!("cp.time.recheck_wrong_rate", Safe, recheck_wrong_rate);
ktest!("cp.time.monotonic_cpus", Safe, monotonic_cpus);
ktest!("cp.time.lapic_every_cpu", Safe, lapic_every_cpu);
ktest!("cp.time.hrtimer_order", Safe, hrtimer_order);
ktest!("cp.time.hrtimer_cancel_rearm", Safe, hrtimer_cancel_rearm);
ktest!("cp.time.hrtimer_drop_periodic", Safe, hrtimer_drop_periodic);
ktest!("cp.time.preempt_busy", Safe, preempt_busy);
ktest!("cp.time.idle_halts", Safe, idle_halts);
ktest!("cp.time.wall_clock", Safe, wall_clock);

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

fn hypervisor() -> bool {
    hal::cpu::live().has("hypervisor")
}

/// Runs `f` on a Normal thread pinned to `cpu` and returns its result.
fn run_on(cpu: usize, f: impl FnOnce() -> R + Send + 'static) -> R {
    let out: Arc<SpinLock<Option<R>>> = Arc::new(SpinLock::named("cp.ktest.out", None));
    let o = out.clone();
    let h = sched::spawn_with("cp-time-test", Class::Normal, SpawnOpts { cpu: Some(cpu), pin: true, rt_prio: sched::RT_PRIO_DEFAULT }, move || {
        let r = f();
        *o.lock() = Some(r);
    })
    .map_err(|e| format!("spawn on cpu {cpu}: {e:?}"))?;
    kassert!(h.join_timeout(60 * SEC), "the test thread on cpu {cpu} did not finish within 60 s");
    sched::reap();
    let r = out.lock().take();
    r.unwrap_or_else(|| Err(format!("no result from cpu {cpu}")))
}

fn online() -> Vec<usize> {
    sched::wait_cpus_online(5 * SEC);
    (0..percpu::cpu_count()).collect()
}

/// The TSC rate is plausible, the HPET gave it on hardware, and the clock agrees with the HPET over 200 ms.
fn calibration() -> R {
    wait_recheck()?;
    let c = time::calibration().ok_or("time::calibrate never ran (acpi::init)")?;
    kassert!((time::clock::MIN_HZ..=time::clock::MAX_HZ).contains(&c.hz), "rate {} Hz", c.hz);
    kassert!(time::tsc_hz() == c.hz, "clock runs at {} Hz, calibration said {}", time::tsc_hz(), c.hz);
    kassert!(time::source() == c.source.code(), "source code {} vs {:?}", time::source(), c.source);
    if !hypervisor() {
        kassert!(matches!(c.source, time::Source::Hpet | time::Source::Reload(_)), "on hardware the HPET gives the rate (spec §7.1), got {:?} ({:?})", c.source, c.tried);
    }
    let Some(p0) = clock_hpet_pair() else {
        if hypervisor() {
            return Err(String::from("QEMU's q35 has an HPET: it should have started"));
        }
        return ktest::skip("no HPET to compare the clock with");
    };
    sched::sleep_ns(200 * MS);
    let p1 = clock_hpet_pair().ok_or("HPET gone")?;
    let hpet_ns = (p1.count.wrapping_sub(p0.count) & 0xFFFF_FFFF) as u128 * p0.fs as u128 / 1_000_000;
    let clock_ns = (p1.ns - p0.ns) as u128;
    let ppm = ((clock_ns as i128 - hpet_ns as i128) * 1_000_000 / (hpet_ns.max(1) as i128)) as i64;
    klog::event!(Info, "cp", "ktest.clock_vs_hpet", ppm = ppm, clock_us = (clock_ns / 1000) as u64, hpet_us = (hpet_ns / 1000) as u64,
        source = c.source.name(), tsc_hz = c.hz, invariant = c.invariant_tsc, span0_ns = p0.span_ns, span1_ns = p1.span_ns,
        recheck = c.recheck.map(|r| r.outcome.name()).unwrap_or("none"), recheck_ppm = c.recheck.map(|r| r.ppm).unwrap_or(0));
    // Hardware: each end is the tightest of several back-to-back reads with interrupts off (D2 of INT-A1d: one
    // pair split by 28 us of something else read as -140 ppm); a microsecond of read jitter is 5 ppm.
    let limit = if hypervisor() { 50_000 } else { 100 };
    kassert!(ppm.abs() <= limit, "clock vs HPET: {ppm} ppm over 200 ms (limit {limit})");
    Ok(())
}

/// A Reload's re-check of the handed-on rate (one second after the calibration) must be over before the
/// clock is compared or the rate is changed by a test.
fn wait_recheck() -> R {
    let t0 = time::now_ns();
    while time::recheck_pending() {
        kassert!(time::now_ns() - t0 < 5 * SEC, "the boot's re-check of the handed-on TSC rate did not finish in 5 s");
        sched::sleep_ns(10 * MS);
    }
    Ok(())
}

/// INT-A1e: a wrong handed-on rate (300 ppm high, as a Reload from an older kernel hands it) is found against
/// the HPET and corrected, and the clock never goes back across either rate change. The right rate is put
/// back at the end. Under a hypervisor the check is judged as on hardware (the boot's own re-check only logs
/// there), with a hypervisor's looser numbers.
fn recheck_wrong_rate() -> R {
    wait_recheck()?;
    let c = time::calibration().ok_or("time::calibrate never ran (acpi::init)")?;
    if time::hpet_read().is_none() {
        if hypervisor() {
            return Err(String::from("QEMU's q35 has an HPET: it should have started"));
        }
        return ktest::skip("no HPET to re-check against");
    }
    let (good_hz, good_src) = (c.hz, c.source);
    let wrong = good_hz + good_hz / 1_000_000 * 300;
    let t0 = time::now_ns();
    time::set_rate_for_test(wrong, time::Source::Reload(good_src.code()));
    let t1 = time::now_ns();
    let r = time::recheck_over(200 * MS, &|ns| sched::sleep_ns(ns), true);
    let t2 = time::now_ns();
    let (hz_after, src_after) = (time::tsc_hz(), time::source());
    let cal_after = time::calibration().map(|c| (c.hz, c.source));
    time::set_rate_for_test(good_hz, good_src);
    let t3 = time::now_ns();
    kassert!(t0 <= t1 && t1 <= t2 && t2 <= t3, "the clock went back across a rate change: {t0} {t1} {t2} {t3}");
    let mut last = t3;
    for cpu in online() {
        let v = hal::smp::call_on(cpu, read_now, 0, time::now_ns, SEC).map_err(|e| format!("call_on {cpu}: {e:?}"))?;
        kassert!(v >= last, "CPU {cpu} read {v} after {last}");
        last = v;
    }
    kassert!(time::tsc_hz() == good_hz && time::source() == good_src.code(), "the right rate is back");
    let r = r.ok_or("the HPET stopped")?;
    klog::event!(Info, "cp", "ktest.recheck", outcome = r.outcome.name(), handed_hz = r.handed_hz, measured_hz = r.measured_hz,
        ppm = r.ppm, good_hz = good_hz, span_ms = r.span_ns / 1_000_000);
    kassert!(r.handed_hz == wrong, "handed {} Hz, the test set {}", r.handed_hz, wrong);
    if hypervisor() && r.outcome != time::recheck::Outcome::Corrected {
        return ktest::skip(&format!("the hypervisor's HPET and TSC disagree: {:?}", r));
    }
    kassert!(r.outcome == time::recheck::Outcome::Corrected, "a 300 ppm error was not corrected: {:?}", r);
    kassert!(hz_after == r.measured_hz && src_after == time::Source::Hpet.code(), "the clock runs at {hz_after} Hz (source {src_after}) after the correction to {}", r.measured_hz);
    kassert!(cal_after == Some((r.measured_hz, time::Source::Hpet)), "the calibration report says {:?}", cal_after);
    // The measured rate is the right one. The rate the clock had was itself within the re-check's 50 ppm of
    // the HPET (or calibrated against it), and 200 ms of tight samples add a few ppm: 60 on hardware, looser
    // under QEMU.
    let limit = if hypervisor() { 5_000 } else { time::recheck::LIMIT_PPM + 10 };
    let off = time::calib::ppm(r.measured_hz, good_hz);
    kassert!(off.abs() <= limit, "measured {} Hz is {off} ppm from the right {good_hz} Hz (limit {limit})", r.measured_hz);
    kassert!((r.ppm - 300).abs() <= limit, "the error was seen as {} ppm, the test made 300", r.ppm);
    Ok(())
}

struct Pair {
    /// Clock ns at the HPET read (the middle of the two clock reads around it).
    ns: u64,
    count: u64,
    fs: u64,
    span_ns: u64,
}

/// The clock and the HPET read together: of 8 tries with interrupts off, the one whose two clock reads around
/// the HPET read lie closest (an SMI, an NMI or a slow bus in one try drops out; `calib::sample`'s rule).
fn clock_hpet_pair() -> Option<Pair> {
    let _irq = hal::cpu::IrqGuard::new();
    let mut best: Option<Pair> = None;
    for _ in 0..8 {
        let a = time::now_ns();
        let (count, fs) = time::hpet_read()?;
        let b = time::now_ns();
        let span_ns = b.saturating_sub(a);
        if best.as_ref().is_none_or(|p| span_ns < p.span_ns) {
            best = Some(Pair { ns: a + span_ns / 2, count, fs, span_ns });
        }
    }
    best
}

fn read_now(_: u64) -> u64 {
    time::now_ns()
}

/// `now_ns` read on every CPU in turn never goes back (synchronised TSCs, one rebase-safe clock).
fn monotonic_cpus() -> R {
    let cpus = online();
    let mut last = 0u64;
    for round in 0..3 {
        for &c in &cpus {
            let v = hal::smp::call_on(c, read_now, 0, time::now_ns, SEC).map_err(|e| format!("call_on {c}: {e:?}"))?;
            kassert!(v >= last, "round {round}: CPU {c} read {v} after {last}");
            last = v;
        }
    }
    Ok(())
}

/// Per CPU: (CPU the callback ran on + 1) | in_hard_irq << 32 | (fired at ns >= deadline) << 33.
static FIRED_ON: PerCpu<AtomicU64> = PerCpu::new(|_| AtomicU64::new(0));
static DEADLINE: PerCpu<AtomicU64> = PerCpu::new(|_| AtomicU64::new(0));

fn record_cpu(slot: usize) {
    let me = percpu::this_cpu() as u64 + 1;
    let irq = (percpu::in_hard_irq() as u64) << 32;
    let not_early = DEADLINE.find_for(slot).is_some_and(|d| time::now_ns() >= d.load(Ordering::Acquire));
    if let Some(f) = FIRED_ON.find_for(slot) {
        f.store(me | irq | (not_early as u64) << 33, Ordering::Release);
    }
}

/// Every online CPU's LAPIC timer runs and fires a timer armed there, on that CPU, from its interrupt.
fn lapic_every_cpu() -> R {
    if time::lapic::config() == time::lapic::Config::Poll {
        return ktest::skip("hrtimer=poll");
    }
    let cpus = online();
    FIRED_ON.cover_possible();
    DEADLINE.cover_possible();
    for &c in &cpus {
        let st = time::lapic::cpu_stats(c).ok_or("no timer state")?;
        kassert!(st.live, "CPU {c}: LAPIC timer not running (error code {})", st.error);
    }
    kassert!(time::interrupt_driven(), "not interrupt-driven with every CPU live");
    let mut lines = Vec::new();
    for &c in &cpus {
        FIRED_ON.get_for(c).ok_or("slot")?.store(0, Ordering::Release);
        let before = time::lapic::cpu_stats(c).map(|s| s.irqs).unwrap_or(0);
        let r = run_on(c, move || {
            let t = HrTimer::new(record_cpu, c);
            let d = time::now_ns() + MS;
            DEADLINE.get_for(c).ok_or("slot")?.store(d, Ordering::Release);
            t.arm(d);
            kassert!(t.cpu() == Some(c), "armed on CPU {c}, queued for {:?}", t.cpu());
            let end = time::now_ns() + 2 * SEC;
            while FIRED_ON.get_for(c).is_some_and(|f| f.load(Ordering::Acquire) == 0) && time::now_ns() < end {
                sched::sleep_ns(MS);
            }
            Ok(())
        });
        r?;
        let v = FIRED_ON.get_for(c).map(|f| f.load(Ordering::Acquire)).unwrap_or(0);
        let after = time::lapic::cpu_stats(c).map(|s| s.irqs).unwrap_or(0);
        kassert!(v != 0, "CPU {c}: the timer never fired");
        kassert!((v & 0xFFFF_FFFF) as usize == c + 1, "CPU {c}: fired on CPU {}", (v & 0xFFFF_FFFF) as i64 - 1);
        kassert!(v & (1 << 32) != 0, "CPU {c}: the callback did not run in the timer interrupt");
        kassert!(v & (1 << 33) != 0, "CPU {c}: fired before its deadline");
        kassert!(after > before, "CPU {c}: no LAPIC timer interrupt counted");
        lines.push(format!("{c}:{}", after - before));
    }
    let kinds: Vec<&str> = cpus.iter().filter_map(|&c| time::lapic::cpu_stats(c).and_then(|s| s.kind)).map(|k| k.name()).collect();
    // hw_info's lines (section interrupts): one per CPU after the clock, wall and queue lines.
    let lines_hw = time::summary();
    kassert!(lines_hw.len() == cpus.len() + 3 && lines_hw.iter().any(|l| l.starts_with("time lapic cpu=0 mode=")), "hw_info time lines: {lines_hw:?}");
    klog::event!(Info, "cp", "ktest.lapic_every_cpu", cpus = cpus.len(), irqs = lines.join(","), mode = kinds.first().copied().unwrap_or(""));
    Ok(())
}

static PERIODIC: IrqSpinLock<Option<HrTimer>> = IrqSpinLock::named("cp.ktest.periodic", None);
static PERIODIC_FIRED: AtomicU64 = AtomicU64::new(0);

fn periodic_rearm(_: usize) {
    PERIODIC_FIRED.fetch_add(1, Ordering::AcqRel);
    if let Some(t) = PERIODIC.lock().as_ref() {
        t.arm_in(100_000);
    }
}

/// A timer re-arming itself every 100 us on the last CPU is dropped from this one, 40 times: the drop never
/// gives up waiting, the timer count comes back, and the timer never fires again (finding of the CP-5 check).
fn hrtimer_drop_periodic() -> R {
    let cpu = *online().last().ok_or("no CPU")?;
    let before = time::hrtimer::count();
    let gave_up = time::hrtimer::DROP_WAIT_GAVE_UP.get();
    for round in 0..40u32 {
        PERIODIC_FIRED.store(0, Ordering::Release);
        run_on(cpu, || {
            let t = HrTimer::new(periodic_rearm, 0);
            // In the static before the first arm: its callback finds it there to re-arm.
            *PERIODIC.lock() = Some(t.clone());
            t.arm_in(100_000);
            Ok(())
        })?;
        let end = time::now_ns() + 2 * SEC;
        while PERIODIC_FIRED.load(Ordering::Acquire) < 5 && time::now_ns() < end {
            sched::sleep_ns(MS / 4);
        }
        kassert!(PERIODIC_FIRED.load(Ordering::Acquire) >= 5, "round {round}: the periodic timer fired {} times in 2 s", PERIODIC_FIRED.load(Ordering::Acquire));
        // Take it out under the lock, drop it outside (the callback takes the same lock).
        let t = PERIODIC.lock().take();
        drop(t);
        let fired = PERIODIC_FIRED.load(Ordering::Acquire);
        sched::sleep_ns(2 * MS);
        let later = PERIODIC_FIRED.load(Ordering::Acquire);
        kassert!(later == fired, "round {round}: fired {} more times after its drop", later - fired);
        kassert!(time::hrtimer::count() == before, "round {round}: {} timers exist, {before} before", time::hrtimer::count());
    }
    kassert!(time::hrtimer::DROP_WAIT_GAVE_UP.get() == gave_up, "a drop gave up waiting");
    Ok(())
}

static ORDER: IrqSpinLock<Vec<usize>> = IrqSpinLock::named("cp.ktest.order", Vec::new());

fn record_order(i: usize) {
    let mut o = ORDER.lock();
    // Room was reserved before arming (no allocation in the interrupt).
    if o.len() < o.capacity() {
        o.push(i);
    }
}

/// Timers armed on one CPU fire in deadline order, equal deadlines in arm order.
fn hrtimer_order() -> R {
    let cpu = *online().last().ok_or("no CPU")?;
    run_on(cpu, || {
        const N: usize = 24;
        let mut room: Vec<usize> = Vec::with_capacity(N);
        core::mem::swap(&mut *ORDER.lock(), &mut room);
        let ts: Vec<HrTimer> = (0..N).map(|i| HrTimer::new(record_order, i)).collect();
        let base = time::now_ns() + 5 * MS;
        // Deadlines in a shuffled order with ties: (i * 7) % 8 steps of 300 us.
        let deadline = |i: usize| base + ((i * 7) % 8) as u64 * 300_000;
        for (i, t) in ts.iter().enumerate() {
            t.arm(deadline(i));
        }
        let end = time::now_ns() + 3 * SEC;
        while ORDER.lock().len() < N && time::now_ns() < end {
            sched::sleep_ns(MS);
        }
        let got: Vec<usize> = ORDER.lock().clone();
        let mut want: Vec<usize> = (0..N).collect();
        want.sort_by_key(|&i| (deadline(i), i));
        kassert!(got.len() == N, "{} of {N} timers fired", got.len());
        kassert!(got == want, "fired {got:?}, wanted {want:?}");
        Ok(())
    })
}

static COUNT: AtomicUsize = AtomicUsize::new(0);
static LATE_OK: AtomicBool = AtomicBool::new(true);
static SELF_T: IrqSpinLock<Option<HrTimer>> = IrqSpinLock::named("cp.ktest.self_timer", None);
static NEXT_DL: AtomicU64 = AtomicU64::new(0);
static FIRED_AT: AtomicU64 = AtomicU64::new(0);

fn count(_: usize) {
    COUNT.fetch_add(1, Ordering::AcqRel);
    FIRED_AT.store(time::now_ns(), Ordering::Release);
}

fn periodic(_: usize) {
    let n = COUNT.fetch_add(1, Ordering::AcqRel) + 1;
    if time::now_ns() < NEXT_DL.load(Ordering::Acquire) {
        LATE_OK.store(false, Ordering::Release);
    }
    if n < 10 {
        let d = time::now_ns() + MS;
        NEXT_DL.store(d, Ordering::Release);
        let t = SELF_T.lock().clone();
        if let Some(t) = t {
            t.arm(d);
        }
    }
}

fn wait_count(n: usize, limit_ns: u64) -> bool {
    let end = time::now_ns() + limit_ns;
    while COUNT.load(Ordering::Acquire) < n {
        if time::now_ns() > end {
            return false;
        }
        sched::sleep_ns(MS / 2);
    }
    true
}

/// Cancel before the deadline: nothing fires; re-arming moves the deadline either way; a callback re-arming its
/// own timer keeps firing; a deadline in the past fires at once.
fn hrtimer_cancel_rearm() -> R {
    let slack = if hypervisor() { 2 * SEC } else { 50 * MS };
    // Cancelled before it was due.
    COUNT.store(0, Ordering::Release);
    let a = HrTimer::new(count, 0);
    a.arm_in(5 * MS);
    kassert!(a.is_armed() && a.cancel(), "cancel of an armed timer");
    sched::sleep_ns(20 * MS);
    kassert!(COUNT.load(Ordering::Acquire) == 0, "a cancelled timer fired");
    // Moved later: it does not fire at the first deadline.
    let t0 = time::now_ns();
    a.arm(t0 + 2 * MS);
    a.arm(t0 + 30 * MS);
    kassert!(a.deadline() == Some(t0 + 30 * MS), "re-arm did not move the deadline");
    kassert!(wait_count(1, 30 * MS + slack), "the moved timer never fired");
    kassert!(FIRED_AT.load(Ordering::Acquire) >= t0 + 30 * MS, "fired at the old (earlier) deadline");
    // Moved earlier.
    COUNT.store(0, Ordering::Release);
    let t0 = time::now_ns();
    a.arm(t0 + 10 * SEC);
    a.arm(t0 + 2 * MS);
    kassert!(wait_count(1, 2 * MS + slack), "the timer moved earlier waited for its old deadline");
    // A past deadline fires at once.
    COUNT.store(0, Ordering::Release);
    a.arm(time::now_ns().saturating_sub(MS));
    kassert!(wait_count(1, slack), "a deadline in the past did not fire at once");
    // Re-armed from its own callback, ten times, never early.
    COUNT.store(0, Ordering::Release);
    LATE_OK.store(true, Ordering::Release);
    let p = HrTimer::new(periodic, 0);
    *SELF_T.lock() = Some(p.clone());
    NEXT_DL.store(time::now_ns() + MS, Ordering::Release);
    p.arm(NEXT_DL.load(Ordering::Acquire));
    let ok = wait_count(10, 10 * MS + 10 * slack);
    *SELF_T.lock() = None;
    kassert!(ok, "the self re-arming timer fired {} of 10 times", COUNT.load(Ordering::Acquire));
    sched::sleep_ns(5 * MS);
    kassert!(COUNT.load(Ordering::Acquire) == 10, "fired {} times, wanted exactly 10", COUNT.load(Ordering::Acquire));
    kassert!(LATE_OK.load(Ordering::Acquire), "a re-armed callback came before its deadline");
    kassert!(!p.is_armed() && !a.is_armed(), "timers still armed");
    Ok(())
}

/// Two fair threads pinned to one CPU spin without any preemption point: both advance in (nearly) every 50 ms
/// window, because the tick fires there by interrupt and the interrupt exit switches (CP-5 + SC-3/SC-4).
fn preempt_busy() -> R {
    if !hal::vectors::irq_exit_hook_installed() {
        return ktest::skip("no preemption at interrupt exit: sched::timers::tick_preempts() is false (change request 1 in plan/notes/cp-CP-5.md)");
    }
    let cpus = online();
    if cpus.len() < 2 {
        return ktest::skip("needs two CPUs (one to spin on, one to watch)");
    }
    let target = *cpus.last().ok_or("no CPU")?;
    let me = sched::current_cpu();
    kassert!(target != me || cpus.len() > 2, "watcher and target on one CPU");
    let target = if target == me { cpus[cpus.len() - 2] } else { target };
    let stop = Arc::new(AtomicBool::new(false));
    let counts: Vec<Arc<AtomicU64>> = (0..2).map(|_| Arc::new(AtomicU64::new(0))).collect();
    let switches0 = hal::vectors::irq_exit_switches();
    let mut hs = Vec::new();
    for c in &counts {
        let (stop, c) = (stop.clone(), c.clone());
        let h = sched::spawn_with("cp-spinner", Class::Normal, SpawnOpts { cpu: Some(target), pin: true, rt_prio: sched::RT_PRIO_DEFAULT }, move || {
            let end = time::now_ns() + 20 * SEC;
            // No preemption point in here: no sleep, no yield, no cond_resched, no lock.
            while !stop.load(Ordering::Relaxed) && time::now_ns() < end {
                c.fetch_add(1, Ordering::Relaxed);
                core::hint::spin_loop();
            }
        })
        .map_err(|e| format!("spawn: {e:?}"))?;
        hs.push(h);
    }
    const WINDOWS: usize = 10;
    let window = if hypervisor() { 100 * MS } else { 50 * MS };
    let mut both = 0;
    // Let both reach the CPU once before counting windows.
    sched::sleep_ns(window);
    let mut last: Vec<u64> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    for _ in 0..WINDOWS {
        sched::sleep_ns(window);
        let now: Vec<u64> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        if now.iter().zip(&last).all(|(n, l)| n > l) {
            both += 1;
        }
        last = now;
    }
    stop.store(true, Ordering::Release);
    for h in &hs {
        kassert!(h.join_timeout(30 * SEC), "a spinner did not stop");
    }
    sched::reap();
    let totals: Vec<u64> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let switches = hal::vectors::irq_exit_switches() - switches0;
    klog::event!(Info, "cp", "ktest.preempt_busy", cpu = target, windows_both = both, windows = WINDOWS, totals = format!("{totals:?}"), irq_switches = switches);
    let need = if hypervisor() { WINDOWS / 2 } else { WINDOWS - 1 };
    kassert!(both >= need, "both spinners advanced in only {both} of {WINDOWS} windows (CPU {target}, totals {totals:?})");
    kassert!(switches > 0, "no switch at an interrupt exit");
    if !hypervisor() {
        let (a, b) = (totals[0].max(1), totals[1].max(1));
        kassert!(a.max(b) / a.min(b) < 4, "uneven shares on CPU {target}: {totals:?}");
    }
    Ok(())
}

/// Idle CPUs halt (IF=1) instead of one spinning for timers: no poll-duty CPU once timers are interrupt-driven.
/// Sampled 40 times over 200 ms: no CPU holds poll duty in most samples, and every other CPU is seen halted in
/// at least half of them (hardware; a quarter under QEMU) — other threads wake CPUs now and then.
fn idle_halts() -> R {
    if !time::interrupt_driven() {
        return ktest::skip("timers are polled (hrtimer=poll or a CPU without a LAPIC timer)");
    }
    let cpus = online();
    if cpus.len() < 2 {
        return ktest::skip("needs a second CPU to watch");
    }
    let me = sched::current_cpu();
    const SAMPLES: u32 = 40;
    let mut halted = alloc::vec![0u32; cpus.len()];
    let mut duty = 0u32;
    for _ in 0..SAMPLES {
        sched::sleep_ns(5 * MS);
        if sched::duty_cpu().is_some() {
            duty += 1;
        }
        for &c in &cpus {
            if c != me && sched::last_idle_entry(c).is_some_and(|e| e.halted_now && e.interrupts_on) {
                halted[c] += 1;
            }
        }
    }
    let shares: Vec<String> = cpus.iter().map(|&c| format!("{c}:{}", halted[c])).collect();
    klog::event!(Info, "cp", "ktest.idle_halts", cpus = cpus.len(), duty_samples = duty, samples = SAMPLES, halted = shares.join(","));
    if duty > SAMPLES / 2 {
        return ktest::skip(&format!("a CPU held poll duty in {duty} of {SAMPLES} samples: SC's idle loop still spins for the pollers (change request 1 in plan/notes/cp-CP-5.md)"));
    }
    let need = if hypervisor() { SAMPLES / 4 } else { SAMPLES / 2 };
    let low: Vec<usize> = cpus.iter().copied().filter(|&c| c != me && halted[c] < need).collect();
    kassert!(low.is_empty(), "CPUs halted in fewer than {need} of {SAMPLES} samples: {low:?} ({shares:?})");
    Ok(())
}

/// The wall clock: the RTC was read (QEMU and the Mini-PC have one), UTC is plausible, the offset is the zone's.
fn wall_clock() -> R {
    let (base, tz, named, err) = time::wall::state();
    let Some(utc) = time::wall::utc_now() else {
        if hypervisor() {
            return Err(format!("no wall clock under QEMU (RTC error {err:?})"));
        }
        return ktest::skip(&format!("no RTC: {err:?}"));
    };
    kassert!((2024..=2100).contains(&utc.year), "UTC year {}", utc.year);
    let off = time::wall::utc_offset_minutes();
    if !named {
        kassert!(off == 60 || off == 120, "Europe/Berlin offset {off} min");
    }
    let a = time::wall::realtime_ns().ok_or("no realtime")?;
    sched::sleep_ns(10 * MS);
    let b = time::wall::realtime_ns().ok_or("no realtime")?;
    kassert!(b >= a + 10 * MS, "realtime did not advance with the monotonic clock: {a} -> {b}");
    klog::event!(Info, "cp", "ktest.wall_clock", utc = time::fmt_dt(&utc, "Z"), offset_min = off, tz = tz.describe(), base = format!("{base:?}"));
    Ok(())
}
