//! The system's watchdog: finding the chip, the boot sequence (FIRED read and cleared, then armed or stopped),
//! the kicker hrtimer, the reporter thread, Quiesce, the deliberate hang.
//!
//! - **Start** (`wdt.start` thread from a Service initcall): waits (bounded) for the PCI scan, looks for the FCH
//!   SMBus function 00:14.0 = 1022:790B revision >= 0x51 (`V1:hw/watchdog.asm:135-154`), maps the ACPI MMIO
//!   window uncached, enables decode and seconds, reads and clears FIRED (`watchdog.fired`), then arms with
//!   the timeout or stops it (`watchdog=off`; a Reload's earlier kernel may have left it running).
//! - **Kicker**: a `time::HrTimer` once a second (timer context: no heap, no lock, no log) runs `Dog::check` and
//!   kicks only when every online CPU is alive (`live`). Not starvable by threads: while CP-5 is missing the
//!   timers are polled by the poll-duty CPU; with CP-5 they fire from the LAPIC timer interrupt.
//! - **Reporter** (`wdt.report`, KernelRt, waits at most 5 s): logs every change between alive and stalled; for
//!   a stalled CPU an NMI sample of the other CPUs (RB-2, `hal::nmi::sample_others`) gives its registers and
//!   backtrace before the reset comes (the kick was withheld; the chip resets `timeout` s after the last kick).
//! - **Quiesce** (`fch-wdt`): Reload stops it (CONFIG `watchdog_reload=grace` keeps it with the grace count);
//!   anything else (reboot) keeps it armed with the grace count and stops the kicks (a safety net). A Reload
//!   whose jump fails arms it again without kicks (the machine has no network then; the reset brings it back).

use crate::config::{self, Config};
use crate::dog::{CpuSource, Dog};
use crate::live::{Live, Policy, Sample};
use crate::regs::{self, ChipError, Regs};
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use ksync::{Once, OnceRef, WaitQueue};

const MS: u64 = 1_000_000;
const SEC: u64 = 1_000_000_000;
/// The kicker's period (V1 kicked once a second, `V1:hw/watchdog.asm:166`).
pub const PERIOD_NS: u64 = SEC;
/// How long the start waits for the PCI scan.
const SCAN_WAIT_NS: u64 = 120 * SEC;

pub const FCH_VENDOR: u16 = 0x1022;
pub const FCH_SMBUS: u16 = 0x790B;
pub const FCH_MIN_REV: u8 = 0x51;

/// What the system watchdog is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    NotStarted = 0,
    /// Waiting for the PCI scan.
    Starting = 1,
    /// No FCH watchdog on this machine (QEMU, another chipset).
    Absent = 2,
    /// CONFIG watchdog=off (or the hypervisor default): found, FIRED read, stopped.
    Off = 3,
    /// Found but not usable (decode refused, disabled by the firmware, the window could not be mapped).
    Unusable = 4,
    Armed = 5,
    /// Stopped by Quiesce (Reload).
    Stopped = 6,
    /// Armed with the grace count, no kicks (Quiesce before a reboot, or a Reload whose jump failed).
    Grace = 7,
}

impl State {
    fn from(v: u8) -> State {
        match v {
            1 => State::Starting,
            2 => State::Absent,
            3 => State::Off,
            4 => State::Unusable,
            5 => State::Armed,
            6 => State::Stopped,
            7 => State::Grace,
            _ => State::NotStarted,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            State::NotStarted => "not started",
            State::Starting => "starting",
            State::Absent => "absent",
            State::Off => "off",
            State::Unusable => "unusable",
            State::Armed => "armed",
            State::Stopped => "stopped",
            State::Grace => "grace",
        }
    }
}

static STATE: AtomicU8 = AtomicU8::new(State::NotStarted as u8);
static SYSTEM: OnceRef<Dog> = OnceRef::new();
static CONFIG: Once<Config> = Once::new();
/// What the boot found: (control, count, fired, was running).
static BOOT: Once<regs::BootStatus> = Once::new();
/// The chip's description for the tool ("00:14.0 1022:790b rev 0x71", or why absent).
static FOUND: Once<String> = Once::new();
/// The system dog runs on the fake register block (a test turned it on under QEMU).
static FAKE: AtomicBool = AtomicBool::new(false);
static QUIESCED: AtomicBool = AtomicBool::new(false);
static TIMER: Once<time::HrTimer> = Once::new();
static REPORT: WaitQueue = WaitQueue::new();
static REPORTER: AtomicBool = AtomicBool::new(false);
static QUIESCE_REGISTERED: AtomicBool = AtomicBool::new(false);
static HANG: AtomicU32 = AtomicU32::new(0);

pub fn state() -> State {
    State::from(STATE.load(Ordering::Acquire))
}
fn set_state(s: State) {
    STATE.store(s as u8, Ordering::Release);
}
/// The start's verdicts land only while it is still the start's turn (a ktest's fake may have taken over).
fn start_verdict(s: State) {
    let _ = STATE.compare_exchange(State::Starting as u8, s as u8, Ordering::AcqRel, Ordering::Acquire);
}
pub fn system() -> Option<&'static Dog> {
    SYSTEM.get()
}
pub fn config() -> Option<&'static Config> {
    CONFIG.get()
}
pub fn boot_status() -> Option<&'static regs::BootStatus> {
    BOOT.get()
}
pub fn found() -> Option<&'static str> {
    if on_fake() {
        return Some("fake register block (ktest)");
    }
    FOUND.get().map(String::as_str)
}
pub fn on_fake() -> bool {
    FAKE.load(Ordering::Acquire)
}

// ---------------------------------------------------------------------------------------------------------
// The scheduler as CPU source

/// The real CPUs: heartbeat and idle record from `sched` (SC-4), probes by reschedule IPI (`sched::probe_cpu`).
pub struct SchedCpus {
    window_ns: u64,
    /// When each CPU was first seen up in hal without a heartbeat or idle record (0: not yet / not such).
    first_up: Vec<AtomicU64>,
}

impl SchedCpus {
    /// `window_ns`: how long a CPU may be up without a heartbeat or idle record before it is judged.
    pub fn new(window_ns: u64) -> SchedCpus {
        SchedCpus { window_ns, first_up: (0..hal::percpu::possible_cpus()).map(|_| AtomicU64::new(0)).collect() }
    }
}

impl CpuSource for SchedCpus {
    fn count(&self) -> usize {
        hal::percpu::possible_cpus()
    }
    fn this_cpu(&self) -> usize {
        hal::percpu::this_cpu()
    }
    fn sample(&self, cpu: usize) -> Sample {
        let hb = sched::heartbeat(cpu);
        let idle = sched::last_idle_entry(cpu);
        // Joined the scheduler: a heartbeat or an idle record (an AP whose join failed has neither; change
        // request: an IRQ-safe `sched::is_online(cpu)`).
        let up = hal::smp::is_online(cpu);
        let mut joined = up && (hb.is_some_and(|h| h != 0) || idle.is_some());
        if up && !joined {
            // Up for a whole window with neither: if the scheduler takes a probe for it (`probe_cpu` accepts only
            // CPUs that joined), it joined and hung before its idle loop -> judged (it stalls). An AP whose join
            // failed is refused and stays offline.
            let now = time::now_ns().max(1);
            let first = self.first_up.get(cpu).map_or(now, |f| match f.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => now,
                Err(v) => v,
            });
            joined = now.saturating_sub(first) > self.window_ns && sched::probe_cpu(cpu);
        }
        Sample {
            online: joined,
            heartbeat: hb.unwrap_or(0),
            idle_now: idle.is_some_and(|e| e.idle_now),
            idle_irq_on: idle.is_some_and(|e| e.interrupts_on),
        }
    }
    fn probe(&self, cpu: usize) -> bool {
        sched::probe_cpu(cpu)
    }
}

// ---------------------------------------------------------------------------------------------------------
// Start

fn read_config() -> (Config, Option<&'static str>) {
    let c = |k: &str| kexec::boot::config(k);
    config::parse(
        c("watchdog").as_deref(),
        c("watchdog_timeout").as_deref(),
        c("watchdog_alive").as_deref(),
        c("watchdog_grace").as_deref(),
        c("watchdog_reload").as_deref(),
        hal::cpu::detect().hypervisor,
    )
}

kreg::initcall!(kreg::Level::Service, "fch-wdt.start", start_thread);
fn start_thread() -> Result<(), &'static str> {
    set_state(State::Starting);
    sched::spawn("wdt.start", sched::Class::Normal, start).map(|_| ()).map_err(|_| "the start thread could not be spawned")
}

/// Whether the PCI scan has published its tree. It publishes every function at once, so one function seen means
/// all are there: 00:14.0 or the host bridge 00:00.0 are cheap lookups; the full list (a copy of every
/// function) only once a second, for a machine without either. Change request: `pci::scanned()`.
fn scanned(round: u64) -> bool {
    pci::function(pci::Loc::new(0, 0, 0x14, 0)).is_some()
        || pci::function(pci::Loc::new(0, 0, 0, 0)).is_some()
        || (round % 20 == 19 && !pci::devices().is_empty())
}

/// The FCH SMBus function, if it is one whose watchdog sits in the ACPI MMIO block.
fn find_fch() -> Result<pci::PciFunction, String> {
    let at = pci::Loc::new(0, 0, 0x14, 0);
    let t0 = time::now_ns();
    // The initcalls run before the kernel's PCI scan: wait for it (bounded).
    let mut round = 0u64;
    let mut said = false;
    while !scanned(round) {
        let waited = time::now_ns().saturating_sub(t0);
        if waited >= SCAN_WAIT_NS {
            klog::event!(Warn, "watchdog", "pci.timeout", waited_ms = waited / MS; "no PCI scan seen: looked anyway");
            break;
        }
        if !said && waited >= SEC {
            said = true;
            klog::event!(Info, "watchdog", "pci.wait", limit_s = SCAN_WAIT_NS / SEC; "waiting for the PCI scan before looking for the FCH");
        }
        sched::sleep_ns(50 * MS);
        round += 1;
    }
    match pci::function(at) {
        Some(f) if f.vendor == FCH_VENDOR && f.device == FCH_SMBUS && f.revision >= FCH_MIN_REV => Ok(f),
        Some(f) => Err(format!("00:14.0 is {:04x}:{:04x} rev {:#x}, not an FCH 1022:790b rev >= 0x51", f.vendor, f.device, f.revision)),
        None => Err(String::from("no function at 00:14.0")),
    }
}

fn start() {
    let (cfg, problem) = read_config();
    let cfg = *CONFIG.get_or_init(|| cfg);
    if let Some(key) = problem {
        klog::event!(Warn, "watchdog", "config.bad", key = key, timeout_s = cfg.timeout_s, alive_s = cfg.alive_s, grace_s = cfg.grace_s; "a bad value: its default is used");
    }
    let f = match find_fch() {
        Ok(f) => f,
        Err(why) => {
            klog::event!(Info, "watchdog", "absent", reason = why.as_str(), wanted = cfg.on; "no FCH watchdog: nothing resets a hung machine");
            FOUND.get_or_init(|| why);
            start_verdict(State::Absent);
            return;
        }
    };
    FOUND.get_or_init(|| format!("{} {:04x}:{:04x} rev {:#x}", f.loc(), f.vendor, f.device, f.revision));
    let io = match hal::mmio::map_device(regs::ACPI_MMIO, regs::WINDOW, hal::paging::CacheType::Uncached) {
        Ok(io) => io,
        Err(e) => {
            klog::event!(Error, "watchdog", "unusable", reason = format!("map {:#x}: {e:?}", regs::ACPI_MMIO); "the FCH's ACPI MMIO window could not be mapped");
            start_verdict(State::Unusable);
            return;
        }
    };
    let r: Box<dyn Regs> = Box::new(regs::Mmio(io));
    boot_with(r, cfg);
}

/// The boot sequence on a register block: decode on, FIRED read and cleared (logged), then armed with the kicker
/// or stopped. Also the entry of the fake system dog (ktests).
fn boot_with(r: Box<dyn Regs>, cfg: Config) {
    let enabled = regs::enable(&*r);
    // FIRED before the verdict (V1 order): a reset by the watchdog is logged even when the chip now refuses.
    let st = regs::read_and_clear_fired(&*r);
    BOOT.get_or_init(|| st);
    let pm4 = r.pm_read8(regs::PM_ISACONTROL);
    klog::event!(Info, "watchdog", "found", chip = found().unwrap_or("?"), control = klog::Hex(st.ctl as u64), count = st.count,
        was_running = st.was_running, fired = st.fired, pm0 = klog::Hex(r.pm_read8(0) as u64), pm3 = klog::Hex(r.pm_read8(3) as u64),
        mmio_en = pm4 & regs::PM4_MMIOEN != 0, fake = on_fake());
    if !on_fake() {
        let s5 = regs::s5_reset_status(&*r);
        klog::event!(Info, "watchdog", "reset_status", value = klog::Hex(s5 as u64), reasons = regs::s5_reset_reasons(s5).as_str();
            "FCH S5_RESET_STATUS: why the last start happened (sticky, not cleared)");
    }
    if st.fired {
        klog::event!(Warn, "watchdog", "fired", control = klog::Hex(st.ctl as u64); "the last start ended by the watchdog (FIRED was set; now cleared)");
    }
    if let Err(e) = enabled {
        klog::event!(Error, "watchdog", "unusable", reason = e.text(); "the watchdog stays off");
        set_state(State::Unusable);
        return;
    }
    if pm4 & regs::PM4_MMIOEN == 0 {
        klog::event!(Warn, "watchdog", "mmio_en.off", pm4 = klog::Hex(pm4 as u64);
            "PM 0x04 bit 1 (ACPI MMIO decode) reads clear: Linux would not use 0xFED80B00; V1 did and it worked, so armed anyway");
    }
    if !cfg.on {
        regs::stop(&*r);
        klog::event!(Info, "watchdog", "off", from_config = cfg.on_from_config, was_running = st.was_running;
            "watchdog off (CONFIG watchdog=off, or the default under a hypervisor)");
        set_state(State::Off);
        return;
    }
    let window = cfg.alive_s as u64 * SEC;
    let dog: &'static Dog = Box::leak(Box::new(Dog::new(r, Box::new(SchedCpus::new(window)), time::now_ns, Policy::new(window), cfg.timeout_s)));
    if !SYSTEM.set(dog) {
        klog::event!(Error, "watchdog", "unusable", reason = "a system watchdog exists already");
        return;
    }
    go(dog, &cfg);
}

/// Arms the system dog and starts kicker, reporter and Quiesce.
fn go(dog: &'static Dog, cfg: &Config) {
    if let Err(e) = dog.arm() {
        klog::event!(Error, "watchdog", "unusable", reason = e.text(); "the watchdog stays off");
        set_state(State::Unusable);
        return;
    }
    QUIESCED.store(false, Ordering::Release);
    set_state(State::Armed);
    if !REPORTER.swap(true, Ordering::AcqRel) {
        let opts = sched::SpawnOpts { rt_prio: 90, ..sched::SpawnOpts::default() };
        if sched::spawn_with("wdt.report", sched::Class::KernelRt, opts, reporter).is_err() {
            REPORTER.store(false, Ordering::Release);
            klog::event!(Warn, "watchdog", "reporter.missing"; "stalls are not logged (the watchdog still resets)");
        }
    }
    if !QUIESCE_REGISTERED.swap(true, Ordering::AcqRel) {
        devmodel::register_quiesce(Arc::new(WdtQuiesce));
        static HOOK: fn() = after_failed_jump;
        kexec::tool::set_jump_failed_hook(&HOOK);
    }
    let t = TIMER.get_or_init(|| time::HrTimer::new(tick, 0));
    t.arm_in(PERIOD_NS);
    klog::event!(Info, "watchdog", "armed", timeout_s = cfg.timeout_s, alive_s = cfg.alive_s, grace_s = cfg.grace_s, cpus = dog.cpus(),
        reload = if cfg.reload_grace { "grace" } else { "stop" }, fake = on_fake();
        "kicked once a second while every CPU is alive");
}

/// The kicker (timer context).
fn tick(_: usize) {
    if QUIESCED.load(Ordering::Acquire) {
        return;
    }
    let Some(d) = SYSTEM.get() else { return };
    d.check();
    if d.has_news() {
        REPORT.wake_one();
    }
    if let Some(t) = TIMER.get() {
        t.arm_in(PERIOD_NS);
    }
}

// ---------------------------------------------------------------------------------------------------------
// Reporter

fn reporter() {
    loop {
        let Some(d) = SYSTEM.get() else { return };
        REPORT.wait_until_timeout(|| d.has_news(), 5 * SEC);
        if d.take_news() {
            report(d);
        }
    }
}

/// Logs stalls and recoveries; for new stalls an NMI sample with registers and backtraces.
pub fn report(d: &Dog) {
    let mut fresh = Vec::new();
    for c in 0..d.cpus() {
        let Some(v) = d.view(c) else { continue };
        if !v.live.alive() {
            if d.mark_reported(c, true) {
                fresh.push(v);
            }
        } else if d.mark_reported(c, false) {
            klog::event!(Warn, "watchdog", "recovered", cpu = c, live = v.live.name(), heartbeat = v.heartbeat; "the CPU is alive again; kicks resume");
        }
    }
    if fresh.is_empty() {
        return;
    }
    let cpus: Vec<usize> = fresh.iter().map(|v| v.cpu).collect();
    let left = (d.timeout_s() as u64 * SEC).saturating_sub(d.now().saturating_sub(d.last_kick_ns()));
    klog::event!(Error, "watchdog", "stall", cpus = format!("{cpus:?}"), window_s = d.policy().window_ns / SEC, armed = d.armed(), reset_in_s = left / SEC;
        "no heartbeat and no answer to probes: kicks withheld, the watchdog resets the machine unless it recovers");
    for v in &fresh {
        klog::event!(Error, "watchdog", "stall.cpu", cpu = v.cpu, live = v.live.name(), heartbeat = v.heartbeat, quiet_ms = v.age_ns / MS,
            probe_unanswered_ms = v.probe_pending_ns.map_or(0, |p| p / MS));
    }
    backtraces(&cpus, "stall");
    klog::drain_deferred(usize::MAX);
}

/// NMI-samples the other CPUs (RB-2) and logs registers and backtrace of `cpus` (all when empty).
pub fn backtraces(cpus: &[usize], why: &str) -> Vec<String> {
    let mut out = Vec::new();
    // Busy: another sample (the tool's, a panic's) runs; it ends within its 1 s wait, so a few bounded tries.
    let mut tries = 0;
    let got = loop {
        tries += 1;
        match hal::nmi::sample_others(time::now_ns, SEC) {
            Err(hal::nmi::NmiError::Busy) if tries < 4 => sched::sleep_ns(400 * MS),
            other => break other,
        }
    };
    let r = match got {
        Ok(r) => r,
        Err(e) => {
            klog::event!(Warn, "watchdog", "sample.failed", why = why, error = format!("{e:?}"), tries = tries);
            out.push(format!("nmi sample failed: {e:?}"));
            return out;
        }
    };
    out.push(format!(
        "nmi sample asked={} answered={} send_failed={} us={} late={} spare={}",
        r.asked,
        r.answered,
        r.send_failed,
        r.ns / 1000,
        hal::nmi::late_answers(),
        hal::nmi::spare_nmis()
    ));
    let me = hal::percpu::this_cpu();
    let all: Vec<usize> = if cpus.is_empty() { (0..hal::percpu::cpu_count()).collect() } else { cpus.to_vec() };
    for &c in &all {
        if c == me {
            out.push(format!("cpu {c}: runs this report"));
            continue;
        }
        let Some(mut regs) = hal::nmi::regs(c).filter(|_| hal::nmi::answer(c) == hal::nmi::Answer::Sampled) else {
            // `owed`: our NMIs sent to it that never reached the hook; one that grows over samples means NMIs do
            // not get through to this CPU at all (blocked, or taken elsewhere), `answers` whether it ever answered.
            let (owed, answers) = (hal::nmi::owed(c), hal::nmi::answers(c));
            klog::event!(Error, "watchdog", "stall.silent", cpu = c, why = why, answer = format!("{:?}", hal::nmi::answer(c)), owed = owed,
                answers = answers; "no answer to the NMI");
            out.push(format!("cpu {c}: no answer to the NMI ({:?}, owed {owed}, answers {answers})", hal::nmi::answer(c)));
            continue;
        };
        let f = regs.frame;
        let at = if regs.user { String::from("user") } else { String::from(kexec::crash::symbolize(f.rip, false).as_str()) };
        let irq_on = f.rflags & (1 << 9) != 0;
        klog::event!(Error, "watchdog", "stall.regs", cpu = c, why = why, rip = klog::Hex(f.rip), at = at.as_str(), rsp = klog::Hex(f.rsp),
            rbp = klog::Hex(f.rbp), rflags = klog::Hex(f.rflags), interrupts = irq_on, cr3 = klog::Hex(regs.cr3), user = regs.user);
        let n = kexec::crash::settle(&mut regs.steps, regs.nsteps);
        let mut line = format!("cpu {c}: {at} rip={:#x} interrupts={}", f.rip, if irq_on { "on" } else { "off" });
        for (i, s) in regs.steps[..n].iter().enumerate().skip(1) {
            let (kind, text) = match *s {
                hal::panic::Step::Call(a) => ("call", String::from(kexec::crash::symbolize(a, true).as_str())),
                hal::panic::Step::Guess(a) => ("guess", format!("~{}", kexec::crash::symbolize(a, true).as_str())),
                hal::panic::Step::Trap { vector, rip, user, .. } => {
                    ("trap", format!("[{}] {}", hal::idt::exception_name(vector), if user { String::from("user") } else { String::from(kexec::crash::symbolize(rip, false).as_str()) }))
                }
                hal::panic::Step::None => continue,
            };
            klog::event!(Error, "watchdog", "stall.frame", cpu = c, n = i, kind = kind, at = text.as_str());
            line.push_str(" < ");
            line.push_str(&text);
        }
        out.push(line);
    }
    out
}

// ---------------------------------------------------------------------------------------------------------
// Quiesce, Reload, reboot

struct WdtQuiesce;

impl devmodel::Quiesce for WdtQuiesce {
    fn name(&self) -> &str {
        "fch-wdt"
    }
    /// No lock, no log (the Reload path runs it with interrupts off and the other CPUs parked).
    fn quiesce(&self) {
        quiesce_now(kexec::tool::reload_jumping());
    }
}

/// `reload`: true on a Reload's way out. Returns the state it left.
pub fn quiesce_now(reload: bool) -> State {
    QUIESCED.store(true, Ordering::Release);
    let Some(d) = SYSTEM.get() else { return state() };
    let grace = CONFIG.get().map_or(config::DEFAULT_GRACE_S, |c| c.grace_s);
    let keep = !reload || CONFIG.get().is_some_and(|c| c.reload_grace);
    if !d.armed() && state() != State::Armed {
        return state();
    }
    if keep {
        d.grace(grace);
        set_state(State::Grace);
    } else {
        d.stop();
        set_state(State::Stopped);
    }
    state()
}

/// kexec's hook after a failed jump: devices quiesced, other CPUs halted. Armed without kicks so the reset
/// brings the machine back. No lock (try_event only).
fn after_failed_jump() {
    let Some(d) = SYSTEM.get() else { return };
    if d.arm_unkicked().is_ok() {
        set_state(State::Grace);
        klog::try_event!(Error, "watchdog", "rearmed", timeout_s = d.timeout_s(); "the Reload failed after the quiesce: the watchdog resets the machine");
    }
}

/// Arms the system dog again after a Quiesce (tests; a later `Quiesce::resume`).
pub fn resume() -> Result<(), ChipError> {
    let Some(d) = SYSTEM.get() else { return Err(ChipError::NoChip) };
    let cfg = CONFIG.get().copied().unwrap_or_else(|| read_config().0);
    d.arm()?;
    QUIESCED.store(false, Ordering::Release);
    set_state(State::Armed);
    if let Some(t) = TIMER.get() {
        t.arm_in(PERIOD_NS);
    }
    klog::event!(Info, "watchdog", "resumed", timeout_s = cfg.timeout_s, fake = on_fake());
    Ok(())
}

/// Under a hypervisor only: the system watchdog on a fake register block, armed, with the real kicker, reporter
/// and Quiesce (a ktest's way to run the whole path where QEMU has no FCH). False when a real chip runs or the
/// start found something else to do with it.
pub fn start_fake(fake: Box<dyn Regs>) -> bool {
    if !hal::cpu::detect().hypervisor || SYSTEM.get().is_some_and(|_| !on_fake()) {
        return false;
    }
    if SYSTEM.get().is_some() {
        return resume().is_ok();
    }
    FAKE.store(true, Ordering::Release);
    let (mut cfg, _) = read_config();
    cfg.on = true;
    CONFIG.get_or_init(|| cfg);
    boot_with(fake, cfg);
    SYSTEM.get().is_some()
}

// ---------------------------------------------------------------------------------------------------------
// The deliberate hang

#[inline(never)]
fn wdt_hang_spin(_: u64) -> u64 {
    loop {
        core::hint::spin_loop();
    }
}

fn wdt_hang_halt(_: u64) -> u64 {
    loop {
        hal::cpu::halt();
    }
}

/// How the deliberate hang stops the machine. The Mini-PC's FCH watchdog fired (FIRED set, count 0) after an
/// `All` hang on 20.10. without resetting the machine; V1's passing test (05.10.) stopped one CPU only. The modes
/// tell those cases apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HangMode {
    /// Every CPU spins with interrupts off.
    All,
    /// Only the calling CPU spins with interrupts off; the others run on (V1's `cli; jmp $`).
    One,
    /// Every CPU halts with interrupts off (`cli; hlt`).
    Halt,
}

impl HangMode {
    pub fn parse(s: &str) -> Option<HangMode> {
        match s.trim() {
            "" | "all" => Some(HangMode::All),
            "one" => Some(HangMode::One),
            "halt" => Some(HangMode::Halt),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            HangMode::All => "all",
            HangMode::One => "one",
            HangMode::Halt => "halt",
        }
    }
}

/// Hangs the machine on purpose (Georg's A1 test): logged first, then the CPUs stop as `mode` says. The watchdog
/// (if armed) resets the machine `timeout` s after its last kick.
pub fn deliberate_hang(mode: HangMode) -> ! {
    HANG.fetch_add(1, Ordering::AcqRel);
    let n = hal::percpu::cpu_count();
    klog::event!(Error, "watchdog", "hang", cpus = n, mode = mode.name(), state = state().name(), timeout_s = system().map_or(0, |d| d.timeout_s()),
        cpu = hal::percpu::this_cpu(); "deliberate hang: the CPUs stop with interrupts off; only the watchdog brings the machine back");
    klog::drain_deferred(usize::MAX);
    // Time for the network thread to send the line to the mirror.
    sched::sleep_ns(300 * MS);
    hal::cpu::disable_interrupts();
    let me = hal::percpu::this_cpu();
    if mode != HangMode::One {
        let f: fn(u64) -> u64 = if mode == HangMode::Halt { wdt_hang_halt } else { wdt_hang_spin };
        for c in (0..n).filter(|&c| c != me) {
            let _ = hal::smp::call_on(c, f, 0, time::now_ns, 20 * MS);
        }
    }
    loop {
        if mode == HangMode::Halt {
            hal::cpu::halt();
        } else {
            core::hint::spin_loop();
        }
    }
}

/// Views of every CPU (the tool).
pub fn views() -> Vec<crate::dog::CpuView> {
    SYSTEM.get().map(|d| (0..d.cpus()).filter_map(|c| d.view(c)).collect()).unwrap_or_default()
}

pub fn live_name(l: Live) -> &'static str {
    l.name()
}
