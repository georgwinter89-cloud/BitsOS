//! RB-quiesce ktests: the other CPUs parked before a reboot's or power-off's quiesce
//! (`hal::kexec_stub::park_for_shutdown`), with work running on them. A test device's Quiesce (registered once,
//! a no-op unless a test arms it) records on which CPU it ran, whether the workers on the other CPUs made
//! progress while it ran, and whether it got the lock the workers take. The Safe tests quiesce only that device
//! (`quiesce_matching`) and release the CPUs; the real reboot and power-off run in `rb.quiesce.qemu_action`
//! (QEMU, CONFIG `rb_quiesce_test=`, scenario `host/tests/rb/60-reboot-park.sh`).

use crate::cpu_ktests::{OnBootCpu, all_answer, aps};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use hal::kexec_stub::{self, ParkRefusal, StopError, StopOptions};
use ktest::{TestResult, kassert, ktest};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000 * MS;
const NAME: &str = "rb-quiesce-test";

/// The lock every worker takes in its loop (a driver's register lock, say). The test Quiesce takes it too: a
/// parked CPU never holds it (it parked with preemption count 0), so the Quiesce gets it at once.
static WORK_LOCK: ksync::SpinLock<u64> = ksync::SpinLock::named("rb.quiesce.work", 0);
/// The workers' progress counters while armed, with the workers' end (the Quiesce reads them; workers never
/// take this lock). Past the end the Quiesce is a no-op: a detached load (`load_for_mcp`) never disarms, and
/// the device stays registered for the boot, so later real quiesces must not pay for an old test.
static ARMED: ksync::SpinLock<(u64, Vec<Arc<AtomicU64>>)> = ksync::SpinLock::named("rb.quiesce.armed", (0, Vec::new()));
static REGISTERED: AtomicBool = AtomicBool::new(false);
static STOP: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicUsize = AtomicUsize::new(0);
// What the last armed Quiesce saw.
static Q_CALLS: AtomicU64 = AtomicU64::new(0);
static Q_CPU: AtomicUsize = AtomicUsize::new(usize::MAX);
static Q_RAN: AtomicU64 = AtomicU64::new(0);
static Q_LOCK: AtomicBool = AtomicBool::new(false);

struct TestDevice;

impl devmodel::Quiesce for TestDevice {
    fn name(&self) -> &str {
        NAME
    }

    /// Must not sleep (devmodel): a 2 ms busy wait between the two looks at the workers' counters.
    fn quiesce(&self) {
        let Some((until, prog)) = ARMED.try_lock().map(|g| (g.0, g.1.clone())) else { return };
        if prog.is_empty() || time::now_ns() >= until {
            return;
        }
        let sum = || prog.iter().map(|p| p.load(Ordering::Acquire)).sum::<u64>();
        let before = sum();
        let end = time::now_ns() + SEC;
        let mut got = None;
        while got.is_none() && time::now_ns() < end {
            got = WORK_LOCK.try_lock().map(|mut g| {
                *g += 1;
            });
            core::hint::spin_loop();
        }
        let t = time::now_ns() + 2 * MS;
        while time::now_ns() < t {
            core::hint::spin_loop();
        }
        let ran = sum().wrapping_sub(before);
        let cpu = hal::percpu::this_cpu();
        Q_CPU.store(cpu, Ordering::Release);
        Q_RAN.store(ran, Ordering::Release);
        Q_LOCK.store(got.is_some(), Ordering::Release);
        Q_CALLS.fetch_add(1, Ordering::AcqRel);
        klog::event!(Info, "rb", "quiesce.test", cpu = cpu, others_ran = ran, lock_ok = got.is_some(), workers = prog.len());
    }
}

fn register_once() {
    if !REGISTERED.swap(true, Ordering::AcqRel) {
        devmodel::register_quiesce(Arc::new(TestDevice));
    }
}

/// One busy kernel thread pinned on every AP, interrupts on, taking `WORK_LOCK` in its loop; ends on `STOP` or
/// at `bound_ns` (B4).
struct Load {
    end: u64,
    prog: Vec<Arc<AtomicU64>>,
    threads: Vec<sched::JoinHandle>,
}

fn worker(p: Arc<AtomicU64>, end: u64) {
    while !STOP.load(Ordering::Acquire) && time::now_ns() < end {
        {
            let mut g = WORK_LOCK.lock();
            *g += 1;
            for _ in 0..64 {
                core::hint::spin_loop();
            }
        }
        p.fetch_add(1, Ordering::AcqRel);
        for _ in 0..256 {
            core::hint::spin_loop();
        }
    }
    RUNNING.fetch_sub(1, Ordering::AcqRel);
}

impl Load {
    fn start(bound_ns: u64) -> Result<Load, String> {
        kassert!(RUNNING.load(Ordering::Acquire) == 0, "workers of an earlier run still run");
        STOP.store(false, Ordering::Release);
        let end = time::now_ns() + bound_ns;
        let mut l = Load { end, prog: Vec::new(), threads: Vec::new() };
        for c in aps() {
            let p = Arc::new(AtomicU64::new(0));
            let mine = p.clone();
            let o = sched::SpawnOpts { cpu: Some(c), pin: true, ..sched::SpawnOpts::default() };
            RUNNING.fetch_add(1, Ordering::AcqRel);
            match sched::spawn_with("rb-quiesce-work", sched::Class::Normal, o, move || worker(mine, end)) {
                Ok(t) => {
                    l.prog.push(p);
                    l.threads.push(t);
                }
                Err(e) => {
                    RUNNING.fetch_sub(1, Ordering::AcqRel);
                    l.stop();
                    return Err(format!("spawn a worker on CPU {c}: {e:?}"));
                }
            }
        }
        Ok(l)
    }

    fn total(&self) -> u64 {
        self.prog.iter().map(|p| p.load(Ordering::Acquire)).sum()
    }

    /// Every worker made progress (bounded wait).
    fn all_moving(&self, within_ns: u64) -> Result<(), String> {
        let base: Vec<u64> = self.prog.iter().map(|p| p.load(Ordering::Acquire)).collect();
        let end = time::now_ns() + within_ns;
        loop {
            let stuck: Vec<usize> = self.prog.iter().zip(&base).enumerate().filter(|(_, (p, b))| p.load(Ordering::Acquire) == **b).map(|(i, _)| i + 1).collect();
            if stuck.is_empty() {
                return Ok(());
            }
            if time::now_ns() >= end {
                return Err(format!("workers on CPU(s) {stuck:?} made no progress"));
            }
            sched::sleep_ns(MS);
        }
    }

    fn arm(&self) {
        register_once();
        *ARMED.lock() = (self.end, self.prog.clone());
    }

    fn stop(&mut self) -> bool {
        ARMED.lock().1.clear();
        STOP.store(true, Ordering::Release);
        let mut all = true;
        for t in self.threads.drain(..) {
            all &= t.join_timeout(5 * SEC);
        }
        all
    }
}

impl Drop for Load {
    fn drop(&mut self) {
        self.stop();
    }
}

fn opts(irq_ns: u64, nmi_ns: Option<u64>) -> StopOptions {
    StopOptions { clock: time::now_ns, irq_wait_ns: irq_ns, nmi_wait_ns: nmi_ns }
}

/// The reboot order with work on every AP: park (the live options, as the reboot uses them), quiesce on CPU 0
/// while the workers stand still, the lock they take is free, a sleep works (the reset sequence waits with it),
/// a kernel TLB shootdown is answered by the parked CPUs; released, every worker moves again.
fn park_beside_work() -> TestResult {
    if aps().is_empty() {
        return ktest::skip("one CPU: nothing to park");
    }
    let _boot = OnBootCpu::take()?;
    let n = aps().len();
    let mut load = Load::start(60 * SEC)?;
    load.all_moving(5 * SEC)?;
    load.arm();
    let calls0 = Q_CALLS.load(Ordering::Acquire);
    let park = kexec_stub::park_for_shutdown(&kexec_stub::live_stop_options(time::now_ns), sched::pin_current);
    let rep = park.report();
    // Everything judged after the release, so a failed check never leaves the CPUs parked.
    let (q, cpu) = (devmodel::quiesce_matching(NAME), hal::percpu::this_cpu());
    let t0 = time::now_ns();
    let total0 = load.total();
    sched::sleep_ns(20 * MS);
    let slept = time::now_ns() - t0;
    let moved_in_sleep = load.total().wrapping_sub(total0);
    let tlb = hal::paging::tlb::flush_kernel(0, 0, false);
    let parked = park.parked();
    let refusal = park.refusal().map(ParkRefusal::describe);
    // The release a failed power-off does (`drop(park)` after `PowerOffError`): unpinned, every AP answers.
    drop(park);
    let unpinned = sched::current_thread().is_some_and(|t| t.pinned().is_none());
    let moving = load.all_moving(5 * SEC);
    let joined = load.stop();
    kassert!(parked, "refused: {refusal:?} ({rep:?})");
    kassert!(rep.others == n && rep.by_irq == n && rep.by_nmi == 0, "every AP parked by IPI: {rep:?}");
    kassert!(cpu == 0, "the quiesce ran on CPU {cpu}, not the boot CPU");
    kassert!(q == 1 && Q_CALLS.load(Ordering::Acquire) == calls0 + 1, "the test device was quiesced once ({q})");
    kassert!(Q_CPU.load(Ordering::Acquire) == 0, "its Quiesce ran on CPU {}", Q_CPU.load(Ordering::Acquire));
    kassert!(Q_RAN.load(Ordering::Acquire) == 0, "the workers made {} steps while the Quiesce ran", Q_RAN.load(Ordering::Acquire));
    kassert!(Q_LOCK.load(Ordering::Acquire), "the Quiesce did not get the workers' lock: a parked CPU holds it");
    kassert!(slept >= 20 * MS && slept < 5 * SEC, "a 20 ms sleep beside the parked CPUs took {} ms", slept / MS);
    kassert!(moved_in_sleep == 0, "the workers made {moved_in_sleep} steps while parked");
    kassert!(tlb.unanswered == 0, "a kernel TLB shootdown went unanswered by {} parked CPU(s): {tlb:?}", tlb.unanswered);
    kassert!(unpinned, "the release did not unpin the thread (callers run unpinned)");
    moving?;
    kassert!(joined, "a worker did not end");
    all_answer()
}
ktest!("rb.quiesce.park_beside_work", Safe, park_beside_work);

static DEFER: AtomicUsize = AtomicUsize::new(usize::MAX);

fn test_guard() -> bool {
    hal::percpu::this_cpu() != DEFER.load(Ordering::Acquire)
}

fn always() -> bool {
    true
}

/// A shutdown whose park is refused goes on with the CPUs running: another stop holds the CPUs (Busy), or an AP
/// never parks (its park guard says "not now", no NMI step). The CPUs answer afterwards and the thread is back
/// where it was.
fn refusals_go_on() -> TestResult {
    let Some(&k) = aps().last() else { return ktest::skip("one CPU: nothing to park") };
    let _boot = OnBootCpu::take()?;
    // Busy: a stop of its own holds the CPUs.
    let held = kexec_stub::stop_others(&opts(5 * SEC, None)).map_err(|e| format!("first stop: {e:?}"))?;
    let p = kexec_stub::park_for_shutdown(&opts(100 * MS, None), sched::pin_current);
    let busy = (p.parked(), p.refusal().cloned());
    drop(p);
    drop(held);
    kassert!(busy == (false, Some(ParkRefusal::Stop(StopError::Busy))), "want Busy, got {busy:?}");
    all_answer()?;
    // An AP that never parks.
    let prev = kexec_stub::park_guard();
    DEFER.store(k, Ordering::Release);
    kexec_stub::set_park_guard(test_guard);
    let p = kexec_stub::park_for_shutdown(&opts(100 * MS, None), sched::pin_current);
    let got = (p.parked(), p.refusal().cloned(), p.guard_deferred());
    drop(p);
    DEFER.store(usize::MAX, Ordering::Release);
    kexec_stub::set_park_guard(prev.unwrap_or(always));
    let want = Some(ParkRefusal::Stop(StopError::NotStopped { cpus: alloc::vec![k], nmi_sent: 0 }));
    kassert!(!got.0 && got.1 == want, "want NotStopped([{k}]), got {got:?}");
    kassert!(got.2 > 0, "the deferrals were counted");
    // The shutdown's quiesce then runs beside the running CPUs, as before RB-quiesce.
    register_once();
    kassert!(devmodel::quiesce_matching(NAME) == 1, "the test device is registered");
    kassert!(sched::current_thread().is_some_and(|t| t.pinned().is_none()), "the release unpins the thread (callers run unpinned)");
    all_answer()
}
ktest!("rb.quiesce.refusals_go_on", Safe, refusals_go_on);

/// An AP that parks only by NMI (its guard defers every IPI) makes the shutdown refuse the park (it may hold a
/// lock) and release the CPUs. QEMU only, like `rb.reload.nmi_parks_stuck_ap`.
fn nmi_refused() -> TestResult {
    let Some(&k) = aps().last() else { return ktest::skip("one CPU: nothing to park") };
    let _boot = OnBootCpu::take()?;
    let prev = kexec_stub::park_guard();
    DEFER.store(k, Ordering::Release);
    kexec_stub::set_park_guard(test_guard);
    let p = kexec_stub::park_for_shutdown(&opts(200 * MS, Some(5 * SEC)), sched::pin_current);
    let got = (p.parked(), p.refusal().cloned(), p.report());
    drop(p);
    DEFER.store(usize::MAX, Ordering::Release);
    kexec_stub::set_park_guard(prev.unwrap_or(always));
    kassert!(!got.0 && got.1 == Some(ParkRefusal::ByNmi { cpus: 1, first: Some(k) }), "want ByNmi CPU {k}, got {got:?}");
    all_answer()
}
ktest!("rb.quiesce.nmi_refused", QemuOnly, nmi_refused);

/// The body of change request 1 (`platform::power::park_others`, plan/notes/rb-RB-quiesce.md) word for word,
/// but logging under "rb", so the scenario runs the glue as the desk will paste it before it is pasted.
fn park_others(what: &'static str) -> hal::kexec_stub::ShutdownPark {
    let park = hal::kexec_stub::park_for_shutdown(&hal::kexec_stub::live_stop_options(time::now_ns), sched::pin_current);
    let r = park.report();
    match park.refusal() {
        None => klog::event!(Info, "rb", "cpus.parked", what = what, cpus_stopped = r.others, by_irq = r.by_irq,
            park_ipis = r.ipis, guard_deferred = park.guard_deferred(), stop_us = r.ns / 1_000),
        Some(why) => klog::event!(Warn, "rb", "cpus.park_refused", what = what, error = why.describe(),
            cpus_stopped = r.others, by_nmi = r.by_nmi; "the other CPUs run on: quiescing beside them"),
    }
    park
}

fn wait(ns: u64) {
    sched::sleep_ns(ns)
}

/// QEMU only, with CONFIG `rb_quiesce_test=` (scenario `host/tests/rb/60-reboot-park.sh`), with a busy worker on
/// every AP and the test device armed:
/// - `reboot` / `poweroff`: the RB-quiesce order itself (park, `quiesce_all`, `acpi::power`), what the glue in
///   `platform::power` and MCP's `reboot` does;
/// - `platform-reboot` / `platform-poweroff`: `platform::power::reboot` / `poweroff` (park only once the desk
///   applied the change request of plan/notes/rb-RB-quiesce.md).
/// Comes back only when the power-off failed.
fn qemu_action() -> TestResult {
    let Some(what) = crate::boot::config("rb_quiesce_test") else {
        return ktest::skip("CONFIG rb_quiesce_test= not set (host/tests/rb/60-reboot-park.sh sets it)");
    };
    if !hal::cpu::detect().hypervisor {
        return ktest::skip("QEMU only");
    }
    let load = Load::start(120 * SEC)?;
    load.all_moving(5 * SEC)?;
    load.arm();
    klog::event!(Info, "rb", "quiesce.load", workers = load.prog.len(), action = what.as_str());
    // The workers stay armed through the reset; on a power-off failure the drop stops them.
    match what.trim() {
        "reboot" | "poweroff" => {
            let park = park_others(if what.trim() == "reboot" { "reboot" } else { "poweroff" });
            let devices = devmodel::quiesce_all();
            klog::event!(Info, "rb", "quiesce.all", devices = devices.join(","));
            if what.trim() == "reboot" {
                acpi::power::reboot(&acpi::power::live_reboot_plan(), &wait)
            }
            let e = acpi::power::poweroff(&wait);
            drop(park);
            Err(format!("power-off came back: {e:?}"))
        }
        "platform-reboot" => platform::power::reboot(),
        "platform-poweroff" => Err(format!("power-off came back: {:?}", platform::power::poweroff())),
        other => Err(format!("rb_quiesce_test={other}: reboot, poweroff, platform-reboot or platform-poweroff")),
    }
}
ktest!("rb.quiesce.qemu_action", QemuOnly, qemu_action);

/// CONFIG `rb_quiesce_load_s=N` only (the scenario's MCP run): a busy worker on every AP for N seconds and the
/// test device armed, then returns, so the next MCP `reboot` quiesces beside (or, with RB-quiesce's glue, after
/// parking) running work. Rerunnable: a second call while the workers run is a no-op.
fn load_for_mcp() -> TestResult {
    let Some(secs) = crate::boot::config("rb_quiesce_load_s").and_then(|v| v.trim().parse::<u64>().ok()) else {
        return ktest::skip("CONFIG rb_quiesce_load_s= not set (host/tests/rb/60-reboot-park.sh sets it)");
    };
    if aps().is_empty() {
        return ktest::skip("one CPU: no AP to load");
    }
    if RUNNING.load(Ordering::Acquire) != 0 {
        return Ok(());
    }
    let mut load = Load::start(secs.clamp(1, 600) * SEC)?;
    load.all_moving(5 * SEC)?;
    load.arm();
    klog::event!(Info, "rb", "quiesce.load", workers = load.prog.len(), seconds = secs);
    // Detached: the workers end at their bound, and from then on the test Quiesce is a no-op (`ARMED`'s end).
    load.threads.clear();
    core::mem::forget(load);
    Ok(())
}
ktest!("rb.quiesce.load_for_mcp", Safe, load_for_mcp);

/// A load past its end leaves the test Quiesce a no-op (a detached `load_for_mcp` never disarms it, and the
/// device stays registered for the boot).
fn expired_is_noop() -> TestResult {
    if aps().is_empty() {
        return ktest::skip("one CPU: no AP to load");
    }
    let mut load = Load::start(100 * MS)?;
    load.arm();
    let deadline = load.end + 5 * SEC;
    while RUNNING.load(Ordering::Acquire) != 0 && time::now_ns() < deadline {
        sched::sleep_ns(10 * MS);
    }
    while time::now_ns() < load.end {
        sched::sleep_ns(MS);
    }
    // Armed as a detached load leaves it: the end passed, the counters still there.
    let calls0 = Q_CALLS.load(Ordering::Acquire);
    let q = devmodel::quiesce_matching(NAME);
    let calls = Q_CALLS.load(Ordering::Acquire) - calls0;
    let joined = load.stop();
    kassert!(q == 1, "the test device is registered ({q})");
    kassert!(calls == 0, "an expired load's Quiesce still ran ({calls} time(s))");
    kassert!(joined, "a worker did not end");
    Ok(())
}
ktest!("rb.quiesce.expired_is_noop", Safe, expired_is_noop);
