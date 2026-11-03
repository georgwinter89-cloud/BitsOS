//! RB-3's ktests. QEMU has no FCH: the chip is a `regs::Fake`, the CPUs are real (sched's heartbeats, idle
//! records and probes). Safe ones (rerunnable; on the Mini-PC pinned to CPU 0 over MCP): the chip sequence on the
//! fake, idle CPUs never stalled (kicks every round), a CPU spinning with interrupts off stalls (no kick, its NMI
//! backtrace), the start's verdict. QemuOnly: the whole system path on the fake (kicker hrtimer, Quiesce for
//! reboot and Reload, resume).

use crate::dog::Dog;
use crate::live::{Live, Policy};
use crate::regs::{self, Fake};
use crate::system::{self, SchedCpus, State};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ktest::{TestResult, kassert, ktest};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;

fn wait_started() -> State {
    let t0 = time::now_ns();
    while matches!(system::state(), State::NotStarted | State::Starting) && time::now_ns() - t0 < 20 * SEC {
        sched::sleep_ns(20 * MS);
    }
    system::state()
}

/// FIRED read and cleared, arm, kick, grace, stop on the fake block (the sequence the real chip gets).
fn fake_chip() -> TestResult {
    let f = Fake::new(true, false, Some(time::now_ns));
    regs::enable(&f).map_err(|e| e.text())?;
    kassert!(f.pm(0) & regs::PM0_DECODE != 0 && f.pm(3) == regs::PM3_SECONDS);
    let st = regs::read_and_clear_fired(&f);
    kassert!(st.fired, "FIRED seen");
    kassert!(f.ctl() & regs::FIRED == 0, "FIRED cleared");
    regs::arm(&f, 30).map_err(|e| e.text())?;
    kassert!(f.running() && f.count() == 30 && f.ctl() & regs::POWEROFF == 0);
    let k = f.kicks();
    regs::kick(&f);
    kassert!(f.kicks() == k + 1);
    regs::set_count_and_kick(&f, 240);
    kassert!(f.count() == 240 && f.running());
    regs::stop(&f);
    kassert!(!f.running());
    let d = Fake::new(false, true, None);
    kassert!(regs::enable(&d) == Err(regs::ChipError::Disabled));
    Ok(())
}
ktest!("rb.wdt.fake_chip", Safe, fake_chip);

fn test_dog(fake: &Arc<Fake>, window_ns: u64) -> Dog {
    Dog::new(Box::new(fake.clone()), Box::new(SchedCpus::new(window_ns)), time::now_ns, Policy::new(window_ns), 30)
}

/// Every round for 3 s with a 1 s window: every online CPU is alive (busy ones move, idle ones answer probes),
/// so every round kicks.
fn idle_never_stalled() -> TestResult {
    let fake = Arc::new(Fake::new(false, false, Some(time::now_ns)));
    let d = test_dog(&fake, SEC);
    d.arm().map_err(|e| e.text())?;
    let k0 = fake.kicks();
    let mut rounds = 0;
    let mut idle_seen = 0;
    let t0 = time::now_ns();
    while time::now_ns() - t0 < 3 * SEC {
        let r = d.check();
        if !r.busy {
            rounds += 1;
            kassert!(r.stalled == 0 && r.kicked, "round {rounds}: {r:?}, cpus {:?}", (0..d.cpus()).map(|c| d.view(c).map(|v| v.live)).collect::<alloc::vec::Vec<_>>());
        }
        idle_seen += (0..d.cpus()).filter(|&c| d.view(c).is_some_and(|v| v.live == Live::Idle)).count();
        sched::sleep_ns(100 * MS);
    }
    kassert!(fake.kicks() == k0 + rounds as u64, "a kick per round");
    let judged = (0..d.cpus()).filter(|&c| d.view(c).is_some_and(|v| v.live != Live::Offline)).count();
    kassert!(judged == sched::online_cpus(), "judged {judged} of {} online CPUs", sched::online_cpus());
    klog::event!(Info, "watchdog", "ktest.idle", rounds = rounds, cpus = judged, probes = d.probes(), idle_verdicts = idle_seen);
    Ok(())
}
ktest!("rb.wdt.idle_never_stalled", Safe, idle_never_stalled);

static STUCK: AtomicBool = AtomicBool::new(false);
static SPINNING: AtomicUsize = AtomicUsize::new(0);

/// Runs on another CPU from its SMP call interrupt (interrupts off) until the test lets go; bounded (8 s).
#[inline(never)]
fn wdt_ktest_spin(_: u64) -> u64 {
    SPINNING.fetch_add(1, Ordering::AcqRel);
    let end = time::now_ns() + 8 * SEC;
    while STUCK.load(Ordering::Acquire) && time::now_ns() < end {
        core::hint::spin_loop();
    }
    SPINNING.fetch_sub(1, Ordering::AcqRel);
    0
}

fn unstick() -> TestResult {
    STUCK.store(false, Ordering::Release);
    let t0 = time::now_ns();
    while SPINNING.load(Ordering::Acquire) != 0 {
        kassert!(time::now_ns() - t0 < 10 * SEC, "the spinning CPU did not end its spin");
        sched::sleep_ns(MS);
    }
    Ok(())
}

fn spin_round(d: &Dog, fake: &Fake, k: usize) -> TestResult {
    let t0 = time::now_ns();
    let mut stalled = false;
    while time::now_ns() - t0 < 5 * SEC {
        let r = d.check();
        let v = d.view(k).ok_or("no view")?;
        // Stalled { idle: true } when it was the idle CPU on poll duty (it claims idle, ignores the probes).
        if !v.live.alive() {
            kassert!(!r.kicked, "a stalled CPU: no kick ({r:?})");
            kassert!(d.take_news(), "the stall is news for the reporter");
            stalled = true;
            break;
        }
        // Busy wait: if the spinning CPU holds poll duty, no hrtimer fires (no sleep would end) until it lets go.
        time::delay_ns(100 * MS);
    }
    kassert!(stalled, "CPU {k} spinning with interrupts off was not judged stalled: {:?}", d.view(k));
    for c in (0..d.cpus()).filter(|&c| c != k) {
        let l = d.view(c).map(|v| v.live);
        kassert!(l.is_some_and(|l| l.alive()), "CPU {c} counted stalled ({l:?})");
    }
    let k0 = fake.kicks();
    d.check();
    kassert!(fake.kicks() == k0, "no kick while CPU {k} stalls");
    // The report's NMI sample: the spinning CPU's backtrace, interrupts off.
    let lines = system::backtraces(&[k], "ktest");
    let text = lines.join("\n");
    klog::event!(Info, "watchdog", "ktest.backtrace", cpu = k, text = text.as_str());
    kassert!(text.contains(&format!("cpu {k}: ")) && text.contains("interrupts=off"), "{text}");
    if kexec::crash::symbols().is_some() {
        kassert!(text.contains("wdt_ktest_spin"), "the spin function in its backtrace: {text}");
    }
    Ok(())
}

/// Keeps the test thread on CPU 0, then gives the old pin back.
struct OnCpu0(Option<usize>);

impl OnCpu0 {
    fn take() -> Result<OnCpu0, String> {
        let prev = sched::current_thread().and_then(|t| t.pinned());
        kassert!(sched::pin_current(Some(0)), "the test thread could not move to CPU 0");
        Ok(OnCpu0(prev))
    }
}

impl Drop for OnCpu0 {
    fn drop(&mut self) {
        sched::pin_current(self.0);
    }
}

/// A CPU spinning with interrupts off: stalled within the window (no probe answer), no kick, the others alive;
/// after it lets go, kicks resume.
fn spin_stalls() -> TestResult {
    let n = hal::percpu::cpu_count();
    if n < 2 || sched::online_cpus() < 2 {
        return ktest::skip("one CPU: nothing to stall");
    }
    let k = n - 1;
    // On CPU 0 for the test: from CPU k itself `call_on` would run the spin inline (8 s, interrupts off) and
    // answer Ok(0) (RB-14 fixer: seen in the full ktest run once earlier tests' park had unpinned the runner
    // and the scheduler had moved it to CPU k).
    let _cpu0 = OnCpu0::take()?;
    let fake = Arc::new(Fake::new(false, false, Some(time::now_ns)));
    let d = test_dog(&fake, SEC);
    d.arm().map_err(|e| e.text())?;
    d.check();
    STUCK.store(true, Ordering::Release);
    match hal::smp::call_on(k, wdt_ktest_spin, 0, time::now_ns, 50 * MS) {
        Err(hal::smp::CallError::Late) => {}
        other => {
            STUCK.store(false, Ordering::Release);
            return Err(format!("CPU {k} should be spinning, call_on gave {other:?}"));
        }
    }
    let res = spin_round(&d, &fake, k);
    unstick()?;
    res?;
    // Back: its heartbeat moves again (a probe or its own switches), kicks resume.
    let t0 = time::now_ns();
    loop {
        let r = d.check();
        if r.kicked && d.view(k).is_some_and(|v| v.live.alive()) {
            break;
        }
        kassert!(time::now_ns() - t0 < 5 * SEC, "CPU {k} did not come back: {:?}", d.view(k));
        sched::sleep_ns(50 * MS);
    }
    Ok(())
}
ktest!("rb.wdt.spin_stalls", Safe, spin_stalls);

/// The start reached a verdict; under a hypervisor without a fake: absent or off, never armed.
fn start_verdict() -> TestResult {
    let st = wait_started();
    kassert!(!matches!(st, State::NotStarted | State::Starting), "the start did not finish: {st:?}");
    if hal::cpu::detect().hypervisor && !system::on_fake() {
        kassert!(matches!(st, State::Absent | State::Off), "QEMU has no FCH: {st:?}");
    }
    klog::event!(Info, "watchdog", "ktest.state", state = st.name(), chip = system::found().unwrap_or("?"));
    Ok(())
}
ktest!("rb.wdt.start", Safe, start_verdict);

/// The whole system path on the fake block: armed with the real kicker hrtimer (kicks arrive), the `watchdog`
/// tool's text, Quiesce for a reboot (grace count, still running, no kicks), resume, Quiesce for a Reload
/// (stopped), resume, the panic hook (RB-13: grace count of the panic delay plus margin, no kicks), resume, and
/// stopped at the end (so `hang` refuses again).
fn system_fake() -> TestResult {
    if !hal::cpu::detect().hypervisor {
        return ktest::skip("only under a hypervisor (the real chip is the system's)");
    }
    let st = wait_started();
    if system::system().is_some() && !system::on_fake() {
        return ktest::skip("a real watchdog runs");
    }
    let _ = st;
    let fake = Arc::new(Fake::new(true, false, Some(time::now_ns)));
    kassert!(system::start_fake(Box::new(fake.clone())) || system::on_fake(), "the fake system dog did not start");
    // A rerun: the fake from the first run is the system's; look at that one through the dog.
    let d = system::system().ok_or("no system dog")?;
    kassert!(system::state() == State::Armed, "{:?}", system::state());
    let k0 = d.kicks();
    let t0 = time::now_ns();
    while d.kicks() < k0 + 2 {
        kassert!(time::now_ns() - t0 < 10 * SEC, "the kicker did not kick: kicks {} checks {}", d.kicks(), d.checks());
        sched::sleep_ns(100 * MS);
    }
    let text = crate::tool::lines(false).join("\n");
    kassert!(text.contains("state=armed") && text.contains("fake=yes") && text.contains("cpu 0 live="), "{text}");
    kassert!(crate::tool::hang_refusal(system::state()).is_none());
    // Reboot: kept armed with the grace count, no kicks.
    kassert!(system::quiesce_now(false) == State::Grace);
    let grace = system::config().map_or(240, |c| c.grace_s);
    kassert!(regs::running(d.regs()) && d.regs().count_read() == grace, "grace: control {:#x} count {}", d.regs().ctl_read(), d.regs().count_read());
    let k1 = d.kicks();
    sched::sleep_ns(2500 * MS);
    kassert!(d.kicks() == k1, "no kicks after the quiesce");
    system::resume().map_err(|e| e.text())?;
    // Reload (default stop): stopped.
    let reload_grace = system::config().is_some_and(|c| c.reload_grace);
    let left = system::quiesce_now(true);
    kassert!(left == if reload_grace { State::Grace } else { State::Stopped }, "{left:?}");
    kassert!(reload_grace || !regs::running(d.regs()), "stopped for a Reload");
    system::resume().map_err(|e| e.text())?;
    // RB-17: the Reload's own Quiesce holds the chip (no kicks, still counting) so the crumbs up to the jump land;
    // kexec's before-jump hook stops it after the last one, which the next start then reads as the end.
    let left = system::quiesce_for_jump();
    kassert!(left == State::Grace && regs::running(d.regs()), "held for the jump: {left:?} control {:#x}", d.regs().ctl_read());
    let base = system::crumb_base();
    if !reload_grace {
        let jump = crate::crumb::code(klog::crumb::Stage::ReloadJump as u16, base);
        if let Some(jump) = jump {
            kassert!(d.crumb(jump), "the jump's crumb is kept while held");
        }
        let k4 = d.kicks();
        sched::sleep_ns(1500 * MS);
        kassert!(d.kicks() == k4, "no kicks while held");
        kassert!(system::stop_at_jump() && system::state() == State::Stopped && !regs::running(d.regs()));
        if jump.is_some() {
            let prev = crate::crumb::Previous::read_with(d.regs().count_read(), base, false);
            kassert!(prev.id == Some(klog::crumb::Stage::ReloadJump as u16) && !prev.incomplete(), "{}", prev.text());
        }
    }
    kassert!(!system::stop_at_jump(), "the stop is one-shot");
    // RB-17 fixer: the jump failed after that stop: kexec's failed-jump hook arms the chip again without kicks,
    // and no second stop follows it (the next stop needs a new hold).
    system::after_failed_jump();
    kassert!(system::state() == State::Grace && regs::running(d.regs()), "re-armed after a failed jump: {:?} control {:#x}", system::state(), d.regs().ctl_read());
    let k5 = d.kicks();
    kassert!(!system::stop_at_jump() && regs::running(d.regs()), "no second stop after a failed jump");
    sched::sleep_ns(1500 * MS);
    kassert!(d.kicks() == k5, "no kicks after a failed jump");
    system::resume().map_err(|e| e.text())?;
    kassert!(system::state() == State::Armed && regs::running(d.regs()));
    // RB-13: the panic hook: counting from the panic's delay plus the margin, still running, no kicks.
    let want = kexec::crash::watchdog_secs(90);
    let got = system::panic_grace(want);
    kassert!(got == kexec::crash::Watchdog::Grace { secs: want }, "{got:?}");
    kassert!(system::state() == State::Grace && regs::running(d.regs()) && d.regs().count_read() == want,
        "panic grace: state {:?} control {:#x} count {}", system::state(), d.regs().ctl_read(), d.regs().count_read());
    let k3 = d.kicks();
    sched::sleep_ns(2500 * MS);
    kassert!(d.kicks() == k3, "no kicks after the panic hook");
    system::resume().map_err(|e| e.text())?;
    let k2 = d.kicks();
    let t0 = time::now_ns();
    while d.kicks() == k2 {
        kassert!(time::now_ns() - t0 < 10 * SEC, "no kicks after resume");
        sched::sleep_ns(100 * MS);
    }
    system::quiesce_now(true);
    if regs::running(d.regs()) {
        // watchdog_reload=grace in this CONFIG: stop it for good here.
        d.stop();
    }
    kassert!(crate::tool::hang_refusal(system::state()).is_some(), "hang refuses once it is stopped");
    Ok(())
}
ktest!("rb.wdt.system_fake", QemuOnly, system_fake);

/// RB-16: a breadcrumb in the system dog's count register (the real chip on the Mini-PC, the fake under QEMU):
/// the code is in the counter and decodes back, the watchdog is quiesced last (after the mirror's transport),
/// and `klog::crumb::clear` puts the normal timeout back. Rerunnable; the state is left as found.
fn crumb_in_the_count() -> TestResult {
    let st = wait_started();
    if system::system().is_none() && hal::cpu::detect().hypervisor {
        let fake = Arc::new(Fake::new(false, false, Some(time::now_ns)));
        kassert!(system::start_fake(Box::new(fake)) || system::on_fake(), "the fake system dog did not start");
    }
    let Some(d) = system::system() else { return ktest::skip(&format!("no system watchdog ({})", st.name())) };
    let stopped_before = system::state() != State::Armed;
    if stopped_before {
        if !system::on_fake() {
            return ktest::skip(&format!("the real watchdog is {} (not armed): nothing to write into", system::state().name()));
        }
        system::resume().map_err(|e| e.text())?;
    }
    kassert!(devmodel::place_of("fch-wdt") == devmodel::Place::Final, "the watchdog is not placed last");
    let base = system::crumb_base();
    kassert!(base > d.timeout_s(), "crumb range base {base} not above the timeout {}", d.timeout_s());
    let left = klog::crumb::leave(klog::crumb::Stage::Test, "rb-crumb-test");
    let code = crate::crumb::code(klog::crumb::Stage::Test as u16, base).ok_or("no code")?;
    let count = d.regs().count_read();
    let back = crate::crumb::decode(count, base);
    klog::crumb::clear();
    let restored = d.regs().count_read();
    if stopped_before {
        d.stop();
        system::quiesce_now(true);
    }
    kassert!(left.keep == Some(true), "the keep store did not take the crumb: {left:?}");
    kassert!(count <= code && count + 2 >= code, "count {count}, code {code}");
    kassert!(back == Some(klog::crumb::Stage::Test as u16), "count {count} decodes to {back:?}");
    kassert!(restored <= d.timeout_s(), "the timeout is not back: count {restored}");
    klog::event!(Info, "watchdog", "ktest.crumb", count = count, code = code, base = base, restored = restored, fake = system::on_fake());
    Ok(())
}
ktest!("rb.wdt.crumb", Safe, crumb_in_the_count);
