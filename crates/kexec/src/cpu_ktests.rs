//! RB-5b ktests: stopping the other CPUs before a Reload, and its refusals. The final steps (LAPIC quiet, halt,
//! INIT) have no way back, so only the Reload scenario (`host/tests/rb/20-reload.sh`, `-smp 16`) runs them; here
//! the park step runs and is released, which leaves every AP as it was.

use alloc::format;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use hal::kexec_stub::{self, StopError, StopOptions, StubError};
use ktest::{TestResult, kassert, ktest};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;

fn opts(irq_ns: u64, nmi_ns: Option<u64>) -> StopOptions {
    StopOptions { clock: time::now_ns, irq_wait_ns: irq_ns, nmi_wait_ns: nmi_ns }
}

pub(crate) fn aps() -> Vec<usize> {
    (1..hal::percpu::cpu_count()).collect()
}

fn whoami(_: u64) -> u64 {
    hal::percpu::this_cpu() as u64
}

/// Every AP answers a cross-CPU call again (they left the park loop). `Busy` while an abandoned call still runs.
pub(crate) fn all_answer() -> TestResult {
    for c in aps() {
        let end = time::now_ns() + 5 * SEC;
        loop {
            match hal::smp::call_on(c, whoami, 0, time::now_ns, SEC) {
                Ok(v) => {
                    kassert!(v == c as u64, "CPU {c} answered as CPU {v}");
                    break;
                }
                Err(hal::smp::CallError::Busy) if time::now_ns() < end => sched::sleep_ns(MS),
                Err(e) => return Err(format!("CPU {c} does not answer after the release: {e:?}")),
            }
        }
    }
    Ok(())
}

fn check_on_ap(_: u64) -> u64 {
    match kexec_stub::check_cpus() {
        Ok(()) => 0,
        Err(StubError::NotBootCpu { cpu }) => 1000 + cpu as u64,
        Err(_) => 1,
    }
}

/// The CPU checks of a Reload: fine on the boot CPU, refused on an AP; the jump refuses while any AP has not
/// been sent INIT.
fn cpu_checks() -> TestResult {
    kassert!(kexec_stub::check_cpus().is_ok(), "the boot CPU may stop the others: {:?}", kexec_stub::check_cpus());
    if aps().is_empty() {
        kassert!(kexec_stub::others_stopped(), "one CPU: nothing to stop");
        return ktest::skip("one CPU: the AP refusals need -smp 2 or more");
    }
    kassert!(!kexec_stub::others_stopped(), "APs run: the jump must refuse");
    for c in aps() {
        let r = hal::smp::call_on(c, check_on_ap, 0, time::now_ns, SEC).map_err(|e| format!("CPU {c}: {e:?}"))?;
        kassert!(r == 1000 + c as u64, "on CPU {c} check_cpus must refuse NotBootCpu, got code {r}");
    }
    Ok(())
}
ktest!("rb.reload.cpu_checks", Safe, cpu_checks);

/// Parks every AP with the park IPI and releases them, twice; each answers afterwards.
fn park_release() -> TestResult {
    let n = aps().len();
    if n == 0 {
        return ktest::skip("one CPU: nothing to park");
    }
    for round in 0..2 {
        let st = kexec_stub::stop_others(&opts(5 * SEC, None)).map_err(|e| format!("round {round}: {e:?}"))?;
        let r = st.report();
        kassert!(r.others == n && r.by_irq == n && r.by_nmi == 0, "round {round}: {r:?}");
        kassert!(!kexec_stub::others_stopped(), "parked is not INIT: the jump must still refuse");
        // A second stop while this one holds the CPUs is refused.
        kassert!(kexec_stub::stop_others(&opts(SEC, None)).err() == Some(StopError::Busy), "second stop refused");
        drop(st);
        all_answer()?;
        klog::event!(Info, "kexec", "ktest.parked", round = round, cpus = n, ipis = r.ipis, us = r.ns / 1000);
    }
    Ok(())
}
ktest!("rb.reload.park_release", Safe, park_release);

/// `hal::nmi::sample_others`, retried while another sample (a late TLB round, the watchdog) runs.
fn sample(wait_ns: u64) -> Result<hal::nmi::Round, alloc::string::String> {
    let end = time::now_ns() + 5 * SEC;
    loop {
        match hal::nmi::sample_others(time::now_ns, wait_ns) {
            Ok(r) => return Ok(r),
            Err(hal::nmi::NmiError::Busy) if time::now_ns() < end => sched::sleep_ns(MS),
            Err(e) => return Err(format!("sample: {e:?}")),
        }
    }
}

fn silent() -> Vec<usize> {
    (0..hal::percpu::cpu_count()).filter(|&c| hal::nmi::answer(c) == hal::nmi::Answer::Silent).collect()
}

/// desk fix/nmi (Mini-PC 20.10., kernel 95d2327): an NMI sample while the APs sit in the park loop is answered
/// there, and every AP still answers samples after the release. The park hook used to swallow every NMI of a
/// parked CPU, so the sampler's NMI never reached `hal::nmi`: those slots stayed "asked" and the CPUs were
/// never sampled again until reboot (12 of 15 silent in rb.wdt.spin_stalls and rb.crash.nmi_sample). The
/// sampler in the field was a TLB shootdown that waited for the parked CPUs (`tlb::sample_late` after 8 ms);
/// both that path and a direct sample are checked.
fn park_answers_nmi_sample() -> TestResult {
    let n = aps().len();
    if n == 0 {
        return ktest::skip("one CPU: nothing to park");
    }
    if !hal::nmi::prepared() {
        return Err("the NMI slots were not prepared at boot".into());
    }
    let _boot = OnBootCpu::take()?;
    let before = sample(2 * SEC)?;
    kassert!(before.answered == n && silent().is_empty(), "before the park: {before:?}, silent {:?}", silent());
    let st = kexec_stub::stop_others(&opts(5 * SEC, None)).map_err(|e| format!("park: {e:?}"))?;
    let parked = st.report();
    let during = sample(SEC);
    let silent_during = silent();
    // The field's sampler: a kernel-half shootdown the parked CPUs cannot serve samples them after 8 ms.
    hal::paging::tlb::allow_sample_now();
    let s0 = hal::paging::tlb::late_samples();
    let flush = hal::paging::tlb::flush_kernel_all();
    let tlb_sampled = hal::paging::tlb::late_samples() - s0;
    drop(st);
    all_answer()?;
    let after = sample(2 * SEC)?;
    let silent_after = silent();
    let during = during?;
    kassert!(parked.by_irq == n, "all parked by the IPI: {parked:?}");
    kassert!(during.asked == n && during.answered == n, "a sample while parked: {during:?}, silent {silent_during:?}");
    kassert!(
        after.asked == n && after.answered == n && silent_after.is_empty(),
        "after the release: {after:?}, silent {silent_after:?} (TLB round while parked: {} unanswered, {tlb_sampled} late samples)",
        flush.unanswered
    );
    klog::event!(Info, "kexec", "ktest.park_nmi", cpus = n, tlb_unanswered = flush.unanswered, tlb_samples = tlb_sampled, us = during.ns / 1000);
    Ok(())
}
ktest!("rb.reload.park_answers_nmi_sample", Safe, park_answers_nmi_sample);

static STUCK: AtomicBool = AtomicBool::new(false);
static SPINNING: AtomicUsize = AtomicUsize::new(0);
static SPIN_START: AtomicU64 = AtomicU64::new(0);
static SPIN_END: AtomicU64 = AtomicU64::new(0);

/// (ms from `t0` to the spin's start, ms to its end) for failure messages.
fn spin_times(t0: u64) -> (i64, i64) {
    let ms = |t: u64| if t == 0 { -1 } else { (t as i64 - t0 as i64) / MS as i64 };
    (ms(SPIN_START.load(Ordering::Acquire)), ms(SPIN_END.load(Ordering::Acquire)))
}

/// Spins with interrupts off until the test lets go: an AP that cannot take the park IPI. Bounded (10 s) so a
/// failing test cannot hang the CPU for good.
fn spin_stuck() {
    let _irq = hal::cpu::IrqGuard::new();
    SPINNING.fetch_add(1, Ordering::AcqRel);
    SPIN_START.store(time::now_ns(), Ordering::Release);
    SPIN_END.store(0, Ordering::Release);
    let end = time::now_ns() + 10 * SEC;
    while STUCK.load(Ordering::Acquire) && time::now_ns() < end {
        // The stuck CPU models interrupts off, not a CPU that ignores TLB shootdowns: without this, any kernel
        // shootdown started meanwhile waits up to tlb::GIVE_UP_NS (500 ms) with interrupts off, longer than the
        // park window, and that CPU fails to park too (CP-5f CR1, applied by INT-A1b).
        hal::paging::tlb::poll();
        core::hint::spin_loop();
    }
    SPIN_END.store(time::now_ns(), Ordering::Release);
    SPINNING.fetch_sub(1, Ordering::AcqRel);
}

/// Keeps the test thread on the boot CPU while an AP is stuck, then gives the old pin back: on the stuck AP's
/// queue it would not run until the spin's bound.
pub(crate) struct OnBootCpu(Option<usize>);

impl OnBootCpu {
    pub(crate) fn take() -> Result<OnBootCpu, alloc::string::String> {
        let prev = sched::current_thread().and_then(|t| t.pinned());
        kassert!(sched::pin_current(Some(0)), "the test thread could not move to CPU 0");
        Ok(OnBootCpu(prev))
    }
}

impl Drop for OnBootCpu {
    fn drop(&mut self) {
        sched::pin_current(self.0);
    }
}

/// The stuck AP: its number and the thread that spins there.
struct Stuck {
    cpu: usize,
    thread: sched::JoinHandle,
}

/// Makes the last AP spin with interrupts off. INT-A2a2: the spin runs in a kernel thread pinned there, entered
/// at a scheduling point, so the AP holds no lock while it is stuck. It used to run from an `SMP_CALL` interrupt,
/// which stopped whatever the AP was doing, also while it held a spin lock taken with interrupts on: the fbdev
/// thread on CPU 0 then spun on `display::FRAME` (in `display::sync_vt`) for the whole 10 s bound with
/// preemption off and the test thread on CPU 0 never ran ("call_on gave Ok(0)", "spin count 0"; CPU 0's
/// backtrace sampled by NMI from the spin). The other half was the kernel's single sleep timer sitting in the
/// stuck AP's queue (sched `timers`, fixed there; `sched.timer.sleep_beside_stuck_cpu`).
fn make_stuck() -> Result<Stuck, alloc::string::String> {
    let k = *aps().last().ok_or("no AP")?;
    kassert!(hal::percpu::this_cpu() != k, "the test thread runs on CPU {k}, the one to be made stuck");
    kassert!(SPINNING.load(Ordering::Acquire) == 0, "a spin of an earlier test still runs");
    STUCK.store(true, Ordering::Release);
    let t0 = time::now_ns();
    let o = sched::SpawnOpts { cpu: Some(k), pin: true, ..sched::SpawnOpts::default() };
    let thread = sched::spawn_with("rb-stuck-ap", sched::Class::KernelRt, o, spin_stuck).map_err(|e| {
        STUCK.store(false, Ordering::Release);
        format!("spawn the spinner: {e:?}")
    })?;
    let end = t0 + 5 * SEC;
    while SPINNING.load(Ordering::Acquire) == 0 && time::now_ns() < end {
        sched::sleep_ns(MS);
    }
    let st = Stuck { cpu: k, thread };
    if SPINNING.load(Ordering::Acquire) != 1 {
        let n = SPINNING.load(Ordering::Acquire);
        unstick(st)?;
        return Err(format!("CPU {k} should spin, spin count {n} after {} ms", (time::now_ns() - t0) / MS));
    }
    Ok(st)
}

fn unstick(st: Stuck) -> TestResult {
    STUCK.store(false, Ordering::Release);
    kassert!(st.thread.join_timeout(5 * SEC), "the stuck CPU {} did not end its spin", st.cpu);
    kassert!(SPINNING.load(Ordering::Acquire) == 0, "spin count {} after the join", SPINNING.load(Ordering::Acquire));
    all_answer()
}

/// An AP with interrupts off answers no park IPI: without the NMI step the stop is refused naming it, and the
/// APs that had parked run again. The IPI it got late is counted as stray, not acted on.
fn stuck_ap_refused() -> TestResult {
    if aps().is_empty() {
        return ktest::skip("one CPU: nothing to park");
    }
    let _boot = OnBootCpu::take()?;
    let (stray0, _) = kexec_stub::stray_and_deferred();
    let st = make_stuck()?;
    let k = st.cpu;
    let r = kexec_stub::stop_others(&opts(300 * MS, None));
    let res = match &r {
        Err(StopError::NotStopped { cpus, nmi_sent: 0 }) if cpus.as_slice() == [k] => Ok(()),
        other => Err(format!("want NotStopped([{k}]), got {:?}", other.as_ref().map(|s| s.report()))),
    };
    drop(r);
    unstick(st)?;
    res?;
    let end = time::now_ns() + 2 * SEC;
    while kexec_stub::stray_and_deferred().0 == stray0 {
        kassert!(time::now_ns() < end, "the late park IPI was not seen as stray");
        sched::sleep_ns(MS);
    }
    // And the next stop works again.
    drop(kexec_stub::stop_others(&opts(5 * SEC, None)).map_err(|e| format!("stop after the refusal: {e:?}"))?);
    all_answer()
}
ktest!("rb.reload.stuck_ap_refused", Safe, stuck_ap_refused);

/// The same stuck AP is parked by the NMI step; released, it goes back to its spin and then answers.
fn nmi_parks_stuck_ap() -> TestResult {
    if aps().is_empty() {
        return ktest::skip("one CPU: nothing to park");
    }
    let _boot = OnBootCpu::take()?;
    let n = aps().len();
    let st = make_stuck()?;
    let k = st.cpu;
    let t0 = time::now_ns();
    let r = kexec_stub::stop_others(&opts(200 * MS, Some(5 * SEC)));
    let t_stop = (time::now_ns() - t0) / MS;
    let res = match &r {
        Ok(st) => {
            let rep = st.report();
            if rep.by_nmi == 1 && rep.by_irq == n - 1 { Ok(()) } else { Err(format!("CPU {k} by NMI, the rest by IPI: {rep:?}")) }
        }
        Err(e) => Err(format!("{e:?}")),
    };
    drop(r);
    let t_rel = (time::now_ns() - t0) / MS;
    // INT-A2a: read the spin count, then always unstick before judging: a failed check that returned early left
    // CPU k spinning with interrupts off for the rest of the gate (gate 3: a 10 s klog hole, then us.msd.read hung).
    let spinning = SPINNING.load(Ordering::Acquire);
    unstick(st)?;
    kassert!(spinning == 1, "CPU {k} went back to its spin after the NMI (spin count {spinning}; stop {t_stop} ms, release {t_rel} ms, spin {:?})", spin_times(t0));
    res
}
ktest!("rb.reload.nmi_parks_stuck_ap", QemuOnly, nmi_parks_stuck_ap);

static DEFER: AtomicUsize = AtomicUsize::new(usize::MAX);

fn test_guard() -> bool {
    hal::percpu::this_cpu() != DEFER.load(Ordering::Acquire)
}

/// A park guard that says "not now" on one AP: that AP is not parked (refused, counted as deferred), the
/// others are released.
fn park_guard_defers() -> TestResult {
    let Some(&k) = aps().last() else { return ktest::skip("one CPU: nothing to park") };
    let prev = kexec_stub::park_guard();
    let (_, def0) = kexec_stub::stray_and_deferred();
    DEFER.store(k, Ordering::Release);
    kexec_stub::set_park_guard(test_guard);
    let r = kexec_stub::stop_others(&opts(100 * MS, None));
    let ok = matches!(&r, Err(StopError::NotStopped { cpus, .. }) if cpus.as_slice() == [k]);
    let got = format!("{:?}", r.as_ref().map(|s| s.report()));
    drop(r);
    DEFER.store(usize::MAX, Ordering::Release);
    kexec_stub::set_park_guard(prev.unwrap_or(always));
    kassert!(ok, "want NotStopped([{k}]), got {got}");
    kassert!(kexec_stub::stray_and_deferred().1 > def0, "the deferral was counted");
    all_answer()
}
ktest!("rb.reload.park_guard_defers", Safe, park_guard_defers);

fn always() -> bool {
    true
}

fn ap_refusals(_: u64) -> u64 {
    (kexec_stub::init_allowed() as u64) | (kexec_stub::virtual_wire() as u64) << 1
}

/// The point-of-no-return steps refuse where they would do harm, without writing anything: INIT only from the
/// boot CPU with interrupts off (`init_allowed`, the gate of `Quiet::send_init`), LINT0 back to virtual wire
/// likewise (the LVT stays as it was).
fn init_and_wire_refusals() -> TestResult {
    let lint0 = || hal::apic::read(hal::apic::REG_LVT_LINT0);
    let before = lint0();
    kassert!(hal::cpu::interrupts_enabled(), "a ktest thread runs with interrupts on");
    kassert!(!kexec_stub::init_allowed(), "interrupts on: no INIT");
    kassert!(!kexec_stub::virtual_wire(), "interrupts on: LINT0 not touched");
    kassert!(lint0() == before, "LINT0 unchanged: {before:?} -> {:?}", lint0());
    {
        let _off = hal::cpu::IrqGuard::new();
        kassert!(kexec_stub::init_allowed(), "the boot CPU with interrupts off may send INIT: {:?}", kexec_stub::check_cpus());
    }
    for c in aps() {
        let r = hal::smp::call_on(c, ap_refusals, 0, time::now_ns, SEC).map_err(|e| format!("CPU {c}: {e:?}"))?;
        kassert!(r == 0, "on CPU {c} INIT and virtual wire must refuse (code {r})");
    }
    kassert!(lint0() == before, "LINT0 unchanged after the AP calls");
    Ok(())
}
ktest!("rb.reload.init_and_wire_refusals", Safe, init_and_wire_refusals);
