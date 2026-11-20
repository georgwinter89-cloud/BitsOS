//! The idle self-check of MWAIT states (GP-CPU2): before the idle loop uses a state, one P core and one E core
//! each enter it a few times with a timer armed, so the real machine shows three things Linux takes on trust:
//! the LAPIC timer (TSC deadline) still wakes the core from that state (ARAT), the wake comes within a bound of
//! the firmware's exit latency, and the core really got there (the core C6/C7 residency counters turbostat reads
//! moved). Anything off: every CPU stays in C1 (`sti; hlt`, as before GP-CPU2) and one Warn line says why.
//!
//! Bounded: a guard thread on a third CPU wakes a core that the timer did not wake within `KICK_AFTER_NS` of its
//! deadline (a write to the line MWAIT monitors and the reschedule IPI), and counts that as the timer failing.
//! The judging is pure (`judge`, host-tested); the measuring runs on the machine (`run`).

use super::plan::Deep;

/// Samples per state and CPU that must have been woken by a timer, and the tries allowed for them. The deadline a
/// sample is judged against is the earliest one programmed into this CPU's LAPIC timer (another timer of this CPU,
/// such as the poll thread's, may come before the sample's own; it wakes the core the same way). A sample woken
/// before that by another interrupt, or whose earliest deadline came too soon to reach the state, says nothing about
/// the state's exit and is tried again.
pub const SAMPLES: usize = 8;
pub const MIN_MEASURED: usize = 4;
pub const TRIES: usize = 32;
/// The guard wakes a sample this long after its deadline.
pub const KICK_AFTER_NS: u64 = 20_000_000;
/// The core C6/C7 counters must show at least this share of the slept time for a C6-or-deeper hint. Hardware may
/// demote a request (the firmware's package limit, a busy neighbour); a counter that does not move at all means
/// the state is not reached.
pub const MIN_RESIDENCY_PCT: u64 = 10;

/// One entry into a state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Wake time minus the armed deadline (negative: woken before it by something else).
    pub late_ns: i64,
    /// TSC ticks between just before MWAIT and just after.
    pub slept_tsc: u64,
    /// Core C6 + C7 residency counted meanwhile (TSC ticks); `None`: a counter could not be read.
    pub residency_tsc: Option<u64>,
    /// The guard had to wake it.
    pub kicked: bool,
}

/// Why a state failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The timer did not wake the core: the guard had to.
    TimerMissed,
    /// The median wake came later than the bound.
    SlowWake { median_us: u64, bound_us: u64 },
    /// A residency counter could not be read.
    NoCounter,
    /// The residency counters did not show the state.
    NotReached { pct: u64 },
    /// Too few samples were woken by their own timer.
    Noisy { measured: usize },
    /// No guard could watch the samples (no thread for it, or another measurement did not end): nothing entered.
    NoGuard,
}

impl Fault {
    pub fn name(&self) -> &'static str {
        match self {
            Fault::TimerMissed => "timer_missed",
            Fault::SlowWake { .. } => "slow_wake",
            Fault::NoCounter => "no_counter",
            Fault::NotReached { .. } => "not_reached",
            Fault::Noisy { .. } => "noisy",
            Fault::NoGuard => "no_guard",
        }
    }
    /// Plain German for the report page and the Warn line.
    pub fn german(&self) -> alloc::string::String {
        match self {
            Fault::TimerMissed => "der Wecker hat einen Kern nicht aus dem Tiefschlaf geholt".into(),
            Fault::SlowWake { median_us, bound_us } => alloc::format!("Aufwachen dauert {median_us} µs (erlaubt {bound_us} µs)"),
            Fault::NoCounter => "der Prozessor nennt die Schlafzeit nicht".into(),
            Fault::NotReached { pct } => alloc::format!("der Kern kam nicht in den Tiefschlaf ({pct} % der Zeit)"),
            Fault::Noisy { measured } => alloc::format!("zu viele Störungen beim Messen ({measured} gültige Proben)"),
            Fault::NoGuard => "die Messung ließ sich nicht absichern, also wurde nicht gemessen".into(),
        }
    }
}

/// What a passed state showed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Passed {
    pub median_late_us: u64,
    /// Residency share of the slept time (`None` for a state above core C6, which has no counter to show).
    pub residency_pct: Option<u64>,
}

/// The wake bound of a state: twice its exit latency plus half a millisecond for the interrupt's way in.
pub fn bound_ns(d: &Deep) -> u64 {
    d.latency_ns.saturating_mul(2).saturating_add(500_000)
}

/// How long a sample sleeps: four times the exit latency (past the target residency, so the core goes as deep
/// as it will), at least a millisecond.
pub fn period_ns(d: &Deep) -> u64 {
    d.latency_ns.saturating_mul(4).max(1_000_000)
}

/// Judges the samples of one state on one CPU (pure).
pub fn judge(d: &Deep, samples: &[Sample]) -> Result<Passed, Fault> {
    if samples.iter().any(|s| s.kicked) {
        return Err(Fault::TimerMissed);
    }
    let mut late: alloc::vec::Vec<u64> = samples.iter().filter(|s| s.late_ns >= 0).map(|s| s.late_ns as u64).collect();
    if late.len() < MIN_MEASURED {
        return Err(Fault::Noisy { measured: late.len() });
    }
    late.sort_unstable();
    let median = late[late.len() / 2];
    if median > bound_ns(d) {
        return Err(Fault::SlowWake { median_us: median / 1000, bound_us: bound_ns(d) / 1000 });
    }
    let residency_pct = match d.mwait {
        Some(h) if super::mwait::reaches_core_c6(h) => {
            let measured = samples.iter().filter(|s| s.late_ns >= 0);
            let mut res = 0u64;
            let mut slept = 0u64;
            for s in measured {
                res = res.saturating_add(s.residency_tsc.ok_or(Fault::NoCounter)?);
                slept = slept.saturating_add(s.slept_tsc);
            }
            let pct = res.saturating_mul(100) / slept.max(1);
            if pct < MIN_RESIDENCY_PCT {
                return Err(Fault::NotReached { pct });
            }
            Some(pct.min(100))
        }
        _ => None,
    };
    Ok(Passed { median_late_us: median / 1000, residency_pct })
}

/// How late a wake was against the deadline the LAPIC really had: the sample's own (`own`) or an earlier timer of
/// this CPU (`programmed`). `None` when that deadline came too soon after arming to reach the state (twice its exit
/// latency): such a wake says nothing about the state's exit and is tried again.
pub fn lateness(d: &Deep, armed_at: u64, own: u64, programmed: Option<u64>, woke: u64) -> Option<i64> {
    let deadline = programmed.map_or(own, |p| own.min(p));
    (deadline.saturating_sub(armed_at) >= d.latency_ns.saturating_mul(2)).then(|| woke as i64 - deadline as i64)
}

/// Which CPU of one core type to measure on: one with no timer armed, so nothing but the sample's own timer comes
/// first; else the one whose programmed deadline is furthest away. A CPU with a periodic timer (the poll thread's
/// 1 ms sleep lives on the CPU it runs on) never leaves the 2.1 ms that hint 0x60 needs, so measuring there would
/// end in `Noisy`. `cands`: per CPU its armed timers and programmed deadline (`None`: unknown, taken last).
pub fn quietest(cands: &[(usize, Option<(usize, u64)>)]) -> Option<usize> {
    cands
        .iter()
        .min_by_key(|(c, q)| match q {
            Some((0, _)) => (0, 0, *c),
            Some((_, at)) => (1, u64::MAX - at, *c),
            None => (2, 0, *c),
        })
        .map(|(c, _)| *c)
}

#[cfg(target_os = "none")]
pub(crate) use run::{probe, spawn};

/// On the host nothing can be entered.
#[cfg(not(target_os = "none"))]
pub(crate) fn probe(_d: &Deep) -> Option<alloc::vec::Vec<Sample>> {
    None
}

#[cfg(target_os = "none")]
mod run {
    use super::{Sample, KICK_AFTER_NS, SAMPLES, TRIES};
    use crate::cstate::plan::{Deep, Plan};
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

    const MSR_CORE_C6_RESIDENCY: u32 = 0x3FD;
    const MSR_CORE_C7_RESIDENCY: u32 = 0x3FE;
    const NO_CPU: usize = usize::MAX;
    /// A second sampler waits this long for the first (the boot check runs a few seconds at most).
    const SAMPLER_WAIT_NS: u64 = 20_000_000_000;

    /// What one sampler shares with its own guard and nobody else: the boot check and a ktest's probe each have
    /// theirs, so one's end cannot stop the other's guard and one's kick cannot count as the other's.
    struct Watch {
        in_sample: AtomicUsize,
        deadline: AtomicU64,
        kicks: AtomicU64,
        done: AtomicBool,
    }

    /// One sampler at a time: two would disturb each other's timers and IPIs.
    static SAMPLER: AtomicBool = AtomicBool::new(false);

    struct Sampling;

    impl Sampling {
        fn take() -> Option<Sampling> {
            let t0 = time::now_ns();
            while SAMPLER.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
                if time::now_ns().saturating_sub(t0) > SAMPLER_WAIT_NS {
                    return None;
                }
                crate::sleep_ms(10);
            }
            Some(Sampling)
        }
    }

    impl Drop for Sampling {
        fn drop(&mut self) {
            SAMPLER.store(false, Ordering::Release);
        }
    }

    fn residency() -> Option<u64> {
        let c6 = hal::msr::try_read(MSR_CORE_C6_RESIDENCY)?;
        let c7 = hal::msr::try_read(MSR_CORE_C7_RESIDENCY)?;
        Some(c6.wrapping_add(c7))
    }

    fn nothing(_: usize) {}

    /// One entry into `d` on this CPU with a timer `period_ns` ahead.
    fn sample(w: &Watch, d: &Deep, hint: u8, timer: &time::HrTimer) -> Sample {
        let cpu = hal::percpu::this_cpu();
        let s = {
            let _irq = hal::cpu::IrqGuard::new();
            let armed_at = time::now_ns();
            let own = armed_at.saturating_add(super::period_ns(d));
            timer.arm(own);
            // The LAPIC fires at the earliest deadline of this CPU's queue, which may be another timer's.
            let programmed = time::hrtimer::queue_state(Some(cpu)).map(|(_, p)| p);
            w.deadline.store(programmed.map_or(own, |p| own.min(p)), Ordering::SeqCst);
            let kicks = w.kicks.load(Ordering::SeqCst);
            w.in_sample.store(cpu, Ordering::SeqCst);
            let r0 = residency();
            let t0 = hal::cpu::rdtsc_ordered();
            hal::cpu::mwait::idle(hint);
            let t1 = hal::cpu::rdtsc_ordered();
            let woke = time::now_ns();
            let r1 = residency();
            w.in_sample.store(NO_CPU, Ordering::SeqCst);
            Sample {
                late_ns: super::lateness(d, armed_at, own, programmed, woke).unwrap_or(-1),
                slept_tsc: t1.wrapping_sub(t0),
                residency_tsc: r0.zip(r1).map(|(a, b)| b.wrapping_sub(a)),
                kicked: w.kicks.load(Ordering::SeqCst) != kicks,
            }
        };
        // Interrupts are on again: the timer's interrupt was taken (or it fires now and is cancelled).
        timer.cancel();
        s
    }

    /// The samples of `d` on this CPU: `SAMPLES` woken by their own timer, at most `TRIES` entries.
    fn samples_of(w: &Watch, d: &Deep, hint: u8, timer: &time::HrTimer) -> alloc::vec::Vec<Sample> {
        let mut v = alloc::vec::Vec::new();
        for _ in 0..TRIES {
            let s = sample(w, d, hint, timer);
            v.push(s);
            if s.kicked || v.iter().filter(|s| s.late_ns >= 0).count() >= SAMPLES {
                break;
            }
        }
        v
    }

    /// The guard: wakes a sample its timer left asleep.
    fn guard(w: &Watch) {
        while !w.done.load(Ordering::SeqCst) {
            crate::sleep_ms(2);
            let cpu = w.in_sample.load(Ordering::SeqCst);
            if cpu == NO_CPU || time::now_ns() < w.deadline.load(Ordering::SeqCst).saturating_add(KICK_AFTER_NS) {
                continue;
            }
            w.kicks.fetch_add(1, Ordering::SeqCst);
            hal::cpu::mwait::wake(cpu);
            crate::cpus::send_resched(cpu);
            // One kick per sample: wait until it is over (bounded).
            for _ in 0..500 {
                if w.in_sample.load(Ordering::SeqCst) != cpu {
                    break;
                }
                crate::sleep_ms(1);
            }
        }
    }

    /// The CPUs to measure: per core type (P, then E; one CPU when the types are unknown) the quietest online CPU
    /// of that type (`super::quietest`), and another online CPU for the guard.
    fn cpus() -> Option<(alloc::vec::Vec<usize>, usize)> {
        let online: alloc::vec::Vec<usize> = crate::cpus::online().map(|c| c.cpu).collect();
        let pick = |of: &dyn Fn(usize) -> bool| {
            let cands: alloc::vec::Vec<(usize, Option<(usize, u64)>)> =
                online.iter().copied().filter(|&c| of(c)).map(|c| (c, time::hrtimer::queue_state(Some(c)))).collect();
            super::quietest(&cands)
        };
        let typed = |t: u8| move |c: usize| hal::smp::topology(c).is_some_and(|x| x.core_type == t);
        let mut test: alloc::vec::Vec<usize> = [pick(&typed(hal::smp::CORE_TYPE_PERFORMANCE)), pick(&typed(hal::smp::CORE_TYPE_EFFICIENCY))].into_iter().flatten().collect();
        if test.is_empty() {
            test.extend(pick(&|_| true));
        }
        let guard = online.iter().copied().find(|c| !test.contains(c))?;
        Some((test, guard))
    }

    /// Runs `f` while a guard of its own watches from `guard_cpu`, one sampler at a time. `None`: the guard could
    /// not be started (no thread, no memory) or another sampler did not finish in time; then nothing is entered,
    /// since an unwatched MWAIT whose timer does not wake the core would sleep until some other interrupt.
    fn with_guard<T>(guard_cpu: usize, f: impl FnOnce(&Watch) -> T) -> Option<T> {
        let _one = Sampling::take()?;
        let w = Arc::new(Watch { in_sample: AtomicUsize::new(NO_CPU), deadline: AtomicU64::new(0), kicks: AtomicU64::new(0), done: AtomicBool::new(false) });
        let gw = w.clone();
        let g = crate::spawn_with("idle-check-guard", crate::Class::KernelRt, crate::SpawnOpts { cpu: Some(guard_cpu), pin: true, ..Default::default() }, move || guard(&gw)).ok()?;
        let r = f(&w);
        w.done.store(true, Ordering::SeqCst);
        g.join_timeout(1_000_000_000);
        Some(r)
    }

    /// Measures every state of `p` and decides (thread context; it pins itself while it measures).
    pub(crate) fn run(p: &'static Plan) -> Result<alloc::vec::Vec<(usize, u8, super::Passed)>, (usize, u8, super::Fault)> {
        let Some((test, guard_cpu)) = cpus() else { return Err((0, 0, super::Fault::Noisy { measured: 0 })) };
        let r = with_guard(guard_cpu, |w| {
            let timer = time::HrTimer::new(nothing, 0);
            let mut out = alloc::vec::Vec::new();
            for &c in &test {
                if !crate::pin_current(Some(c)) {
                    return Err((c, 0, super::Fault::Noisy { measured: 0 }));
                }
                for d in &p.deep {
                    let Some(hint) = d.mwait else { continue };
                    match super::judge(d, &samples_of(w, d, hint, &timer)) {
                        Ok(ok) => out.push((c, hint, ok)),
                        Err(f) => return Err((c, hint, f)),
                    }
                }
            }
            Ok(out)
        });
        crate::pin_current(None);
        r.unwrap_or(Err((guard_cpu, 0, super::Fault::NoGuard)))
    }

    /// The samples of one state on this CPU with the guard on another one (ktests; the caller pins itself). `None`
    /// without a second online CPU, without MWAIT for `hint`, or without a guard (`with_guard`).
    pub(crate) fn probe(d: &Deep) -> Option<alloc::vec::Vec<Sample>> {
        let hint = d.mwait?;
        let caps = hal::cpu::mwait::MwaitCaps::live();
        if !caps.usable() || !caps.has_hint(hint) {
            return None;
        }
        let me = hal::percpu::this_cpu();
        let guard_cpu = crate::cpus::online().map(|c| c.cpu).find(|&c| c != me)?;
        let timer = time::HrTimer::new(nothing, 0);
        with_guard(guard_cpu, |w| samples_of(w, d, hint, &timer))
    }

    /// Starts the check in its own thread once every CPU is up and the timers are interrupt-driven.
    pub(crate) fn spawn(p: &'static Plan) {
        let r = crate::spawn("idle-check", crate::Class::Normal, move || {
            crate::wait_cpus_online(10_000_000_000);
            let t0 = time::now_ns();
            while !time::interrupt_driven() && time::now_ns().saturating_sub(t0) < 10_000_000_000 {
                crate::sleep_ms(10);
            }
            crate::cstate::finish_check(p, if time::interrupt_driven() { run(p) } else { Err((0, 0, super::Fault::TimerMissed)) });
        });
        if r.is_err() {
            crate::cstate::finish_check(p, Err((0, 0, super::Fault::NoGuard)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deep(hint: u8, latency_us: u64) -> Deep {
        Deep { kind: 2, port: 0, latency_ns: latency_us * 1000, bm_check: false, mwait: Some(hint) }
    }
    fn s(late_us: i64, slept: u64, res: Option<u64>) -> Sample {
        Sample { late_ns: late_us * 1000, slept_tsc: slept, residency_tsc: res, kicked: false }
    }

    #[test]
    fn a_good_c6_passes() {
        let d = deep(0x21, 127);
        let v: alloc::vec::Vec<Sample> = (0..8).map(|i| s(20 + i, 1_000_000, Some(800_000))).collect();
        assert_eq!(judge(&d, &v), Ok(Passed { median_late_us: 24, residency_pct: Some(80) }));
    }

    #[test]
    fn a_kick_is_a_missed_timer_whatever_else() {
        let d = deep(0x60, 1048);
        let mut v: alloc::vec::Vec<Sample> = (0..8).map(|_| s(30, 1_000_000, Some(900_000))).collect();
        v[3].kicked = true;
        assert_eq!(judge(&d, &v), Err(Fault::TimerMissed));
    }

    #[test]
    fn early_wakes_do_not_count_and_too_few_is_noisy() {
        let d = deep(0x21, 127);
        let mut v: alloc::vec::Vec<Sample> = (0..12).map(|_| s(-200, 1_000_000, Some(0))).collect();
        v.extend((0..3).map(|_| s(10, 1_000_000, Some(500_000))));
        assert_eq!(judge(&d, &v), Err(Fault::Noisy { measured: 3 }));
        v.push(s(10, 1_000_000, Some(500_000)));
        assert!(judge(&d, &v).is_ok(), "four measured; the early ones' zero residency is not held against it");
    }

    #[test]
    fn slow_wake_is_judged_by_the_median() {
        let d = deep(0x60, 1048);
        assert_eq!(bound_ns(&d), 2_596_000);
        let mut v: alloc::vec::Vec<Sample> = (0..8).map(|_| s(3000, 1_000_000, Some(500_000))).collect();
        assert_eq!(judge(&d, &v), Err(Fault::SlowWake { median_us: 3000, bound_us: 2596 }));
        for x in v.iter_mut().take(5) {
            x.late_ns = 100_000;
        }
        assert!(judge(&d, &v).is_ok(), "three slow outliers of eight");
    }

    #[test]
    fn residency_must_show_for_c6_and_deeper_only() {
        let c6 = deep(0x21, 127);
        let v: alloc::vec::Vec<Sample> = (0..8).map(|_| s(10, 1_000_000, Some(50_000))).collect();
        assert_eq!(judge(&c6, &v), Err(Fault::NotReached { pct: 5 }));
        let unread: alloc::vec::Vec<Sample> = (0..8).map(|_| s(10, 1_000_000, None)).collect();
        assert_eq!(judge(&c6, &unread), Err(Fault::NoCounter));
        let c1e = deep(0x01, 2);
        assert_eq!(judge(&c1e, &unread), Ok(Passed { median_late_us: 10, residency_pct: None }), "no counter needed above core C6");
    }

    #[test]
    fn periods_reach_past_the_target_residency() {
        assert_eq!(period_ns(&deep(0x21, 127)), 1_000_000);
        assert_eq!(period_ns(&deep(0x60, 1048)), 4_192_000);
    }

    /// A CPU with a 1 ms periodic timer (the poll thread sleeps 1 ms on the CPU it runs on) never leaves hint 0x60
    /// the 2.1 ms it needs: all 32 tries say nothing and the state fails as `Noisy`, deep idle off for the boot. A
    /// quiet CPU of the same type measures it; `quietest` picks that one (checker finding 5).
    #[test]
    fn a_periodic_timer_hides_c10_and_the_quiet_cpu_is_taken() {
        let d = deep(0x60, 1048);
        let wake_after = 40_000;
        let tries = |periodic: bool| -> alloc::vec::Vec<Sample> {
            let mut now = 123_456u64;
            (0..TRIES)
                .map(|_| {
                    let own = now + period_ns(&d);
                    let programmed = periodic.then(|| (now / 1_000_000 + 1) * 1_000_000);
                    let woke = programmed.map_or(own, |p| own.min(p)) + wake_after;
                    let s = Sample { late_ns: lateness(&d, now, own, programmed.or(Some(own)), woke).unwrap_or(-1), slept_tsc: 1_000_000, residency_tsc: Some(900_000), kicked: false };
                    now = woke + 7_000;
                    s
                })
                .take_while({
                    let mut measured = 0;
                    move |s| {
                        let more = measured < SAMPLES;
                        measured += usize::from(s.late_ns >= 0);
                        more
                    }
                })
                .collect()
        };
        assert_eq!(judge(&d, &tries(true)), Err(Fault::Noisy { measured: 0 }));
        assert_eq!(judge(&d, &tries(false)), Ok(Passed { median_late_us: 40, residency_pct: Some(90) }));
        // CPU 0 (the poll thread's 1 ms sleep), 2 (a timer 5 ms out), 4 (nothing armed), 6 (queue unknown).
        let cands = [(0, Some((1, 1_000_000))), (2, Some((1, 5_000_000))), (4, Some((0, 777))), (6, None)];
        assert_eq!(quietest(&cands), Some(4));
        assert_eq!(quietest(&cands[..2]), Some(2), "the furthest deadline when every CPU has a timer");
        assert_eq!(quietest(&[(6, None), (0, Some((2, 10)))]), Some(0), "an unknown queue is taken last");
        assert_eq!(quietest(&[]), None);
    }

    #[test]
    fn lateness_is_against_the_earliest_deadline_and_needs_room() {
        let d = deep(0x21, 127);
        assert_eq!(lateness(&d, 0, 1_000_000, Some(1_000_000), 1_030_000), Some(30_000));
        assert_eq!(lateness(&d, 0, 1_000_000, Some(400_000), 410_000), Some(10_000), "another timer came first");
        assert_eq!(lateness(&d, 0, 1_000_000, Some(200_000), 210_000), None, "200 us < 2 x 127 us");
        assert_eq!(lateness(&d, 0, 1_000_000, None, 990_000), Some(-10_000), "woken early by something else");
        assert_eq!(Fault::NoGuard.name(), "no_guard");
    }
}
