//! Idle depth (FD-1, SC-9): which hardware sleep state an idle CPU enters.
//!
//! Until FD-1 every idle CPU ran `sti; hlt` (C1): the core stays powered at its voltage. Linux enters the
//! deeper states the firmware lists in `_CST` (acpi_idle); on the Mini-PC (Ryzen 7 PRO 8845HS) C2 = a byte read
//! from I/O port 0x414 (CC6, 18 us exit) and C3 = port 0x415 (350 us). Measured there (plan/notes/sc-FD-1.md):
//! package power of the idle console 4.4 W with C1 only, 3.4 W with this; the single-core clock stays 4.44 GHz
//! either way, video frame drops unchanged.
//!
//! Which states may be used is `plan` (Linux's checks; C1 only under a hypervisor, without ARAT or without an
//! invariant TSC); how one is entered is `entry` (bus-master check before C3 as Linux does for AMD Zen). The
//! choice (Linux `menu`/`teo` in small): the deepest state whose target residency (`residency_factor` times its
//! exit latency) fits both the time to this CPU's next own timer and the predicted idle length (a running
//! average of this CPU's recent idle periods, so a CPU woken every few hundred microseconds by a futex
//! ping-pong stays in C1 and keeps its fast wake-up). CONFIG `idle=deep|c1` (default deep) switches it off.

mod entry;
pub mod fake;
mod plan;

pub use entry::{enter, Entered, IdleHw, BM_STS};
pub use plan::{plan, C1Only, CpuIdle, Deep, Firmware, Plan, Refused, Vendor, MAX_LATENCY_US};

use crate::cpus::CpuSched;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering};

/// Target residency = this many times the exit latency (Linux acpi_idle uses 2; a bit more caution here).
pub const DEFAULT_RESIDENCY_FACTOR: u32 = 4;

/// `Auto` (CONFIG `idle=deep`, the default): choose by timer and prediction; `C1` (`idle=c1`): `sti; hlt`
/// only, the behaviour before FD-1; `Forced`: always the shallowest deep state (measuring build only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Auto = 0,
    C1 = 1,
    Forced = 2,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Auto => "auto",
            Mode::C1 => "c1",
            Mode::Forced => "forced",
        }
    }
}

static MODE: AtomicU8 = AtomicU8::new(Mode::Auto as u8);
static FACTOR: AtomicU32 = AtomicU32::new(DEFAULT_RESIDENCY_FACTOR);
static PLAN: ksync::Once<Plan> = ksync::Once::new();
static DEMOTION_LOGGED: AtomicBool = AtomicBool::new(false);
static REFUSAL_LOGGED: AtomicBool = AtomicBool::new(false);
klog::counter!(pub BM_DEMOTIONS, "sched.idle.bm_demotions");

/// CONFIG `idle=` to a mode (pure; host-tested): `deep` (or missing) -> `Auto`, `c1` -> `C1`. An unknown value
/// keeps the default and says so (`true`), so the caller can warn.
pub fn parse_config(v: Option<&str>) -> (Mode, bool) {
    match v.map(str::trim) {
        None | Some("") | Some("deep") => (Mode::Auto, false),
        Some("c1") => (Mode::C1, false),
        Some(_) => (Mode::Auto, true),
    }
}

/// Reads CONFIG `idle=` (the kernel hands its getter over, before or after `init`).
pub fn configure(get: &dyn Fn(&str) -> Option<String>) {
    let v = get("idle");
    let (m, bad) = parse_config(v.as_deref());
    if bad {
        klog::event!(Warn, "sched", "config.unknown", key = "idle", value = v.unwrap_or_default(); "idle= takes deep|c1; deep kept");
    }
    set_mode(m);
    if m == Mode::C1 {
        klog::event!(Info, "sched", "idle.mode", mode = m.name(), source = "config");
    }
}

/// The firmware facts from the parsed ACPI tables.
pub fn firmware(a: &acpi::Acpi) -> Firmware<'_> {
    let fadt = a.fadt.as_ref();
    let io = |g: Option<acpi::sdt::Gas>| {
        g.filter(|g| g.space == acpi::sdt::Space::Io && !fadt.is_some_and(|f| f.hw_reduced()))
            .and_then(|g| u16::try_from(g.address).ok())
            .filter(|&p| p != 0)
    };
    Firmware {
        states: a.cst.as_ref().map_or(&[][..], |(c, _, _)| &c.states[..]),
        pm_timer: fadt.and_then(|f| f.pm_timer_port()).map(|(p, _)| p),
        pm1_status: [io(fadt.and_then(|f| f.pm1a_evt)), io(fadt.and_then(|f| f.pm1b_evt))],
    }
}

/// Plans once (sched init, after ACPI) and says what it chose and refused. Under a hypervisor the tables are
/// not even looked at.
pub(crate) fn init() {
    let p = PLAN.get_or_init(|| {
        let cpu = CpuIdle::live();
        let tables = if cpu.hypervisor { None } else { acpi::live::tables() };
        let fw = tables.map(firmware);
        plan(&cpu, fw.as_ref(), &mut |port, bytes| acpi::live::check_io(port as u64, bytes, false).is_ok()
            && hal::portio::allowed(port, bytes))
    });
    if let Some((c, sig, phys)) = acpi::live::tables().and_then(|a| a.cst.as_ref()).filter(|_| p.c1_only != Some(C1Only::Hypervisor)) {
        klog::event!(Info, "sched", "idle.cst", table = acpi::bytes::text(sig), phys = klog::Hex(*phys), states = c.states.len(),
            skipped = c.skipped.len(), truncated = c.truncated);
        for (k, why) in &c.skipped {
            klog::event!(Info, "sched", "idle.cst_skipped", element = *k, reason = why.name(); "a malformed _CST element; the others are used");
        }
    }
    log_plan(p, mode());
}

fn log_plan(p: &Plan, m: Mode) {
    for (c, why) in &p.refused {
        klog::event!(Info, "sched", "idle.state_refused", kind = c.kind, entry = alloc::format!("{:?}", c.entry), latency_us = c.latency_us,
            reason = why.name());
    }
    if p.latency_reordered {
        klog::event!(Info, "sched", "idle.latency_reordered"; "firmware latencies out of order, sorted as Linux does");
    }
    for d in &p.deep {
        klog::event!(Info, "sched", "idle.state", kind = d.kind, port = klog::Hex(d.port as u64), latency_us = d.latency_ns / 1000,
            bm_check = d.bm_check);
    }
    match p.c1_only {
        Some(why) => klog::event!(Info, "sched", "idle.c1_only", reason = why.name()),
        None => klog::event!(Info, "sched", "idle.deep", states = p.deep.len(), mode = m.name(), dummy_read = p.dummy_read.is_some()),
    }
}

pub fn mode() -> Mode {
    match MODE.load(Ordering::Relaxed) {
        1 => Mode::C1,
        2 => Mode::Forced,
        _ => Mode::Auto,
    }
}

pub fn set_mode(m: Mode) {
    MODE.store(m as u8, Ordering::Relaxed);
}

pub fn residency_factor() -> u32 {
    FACTOR.load(Ordering::Relaxed)
}

pub fn set_residency_factor(f: u32) {
    FACTOR.store(f.clamp(1, 1000), Ordering::Relaxed);
}

/// The plan made at init (`None` before; for reports).
pub fn current_plan() -> Option<&'static Plan> {
    PLAN.get()
}

/// Which state to enter (pure, host-tested): index into `deep` (rising latency) or `None` for C1. `gap_ns`: time
/// to this CPU's next own timer (`None`: no timer), `predicted_ns`: the expected idle length.
pub fn choose(mode: Mode, deep: &[Deep], gap_ns: Option<u64>, predicted_ns: u64, factor: u32) -> Option<usize> {
    if deep.is_empty() {
        return None;
    }
    match mode {
        Mode::C1 => None,
        Mode::Forced => Some(0),
        Mode::Auto => {
            let budget = gap_ns.map_or(predicted_ns, |g| g.min(predicted_ns));
            deep.iter().rposition(|d| d.latency_ns.saturating_mul(factor as u64) <= budget)
        }
    }
}

/// Next prediction from the last one and the idle period that just ended: an average that drops quickly
/// (one short period halves it) and grows slowly (an eighth of the way), so a burst of short wake-ups keeps
/// the CPU shallow and only steady quiet makes it go deep.
pub fn predict(prev_ns: u64, last_ns: u64) -> u64 {
    if last_ns < prev_ns { prev_ns - (prev_ns - last_ns) / 2 } else { prev_ns + (last_ns - prev_ns) / 8 }
}

#[cfg(bitsos_fd_measure)]
mod measure {
    klog::counter!(pub C1_ENTRIES, "sched.idle.c1_entries");
    klog::counter!(pub C1_NS, "sched.idle.c1_ns");
    klog::counter!(pub C2_ENTRIES, "sched.idle.c2_entries");
    klog::counter!(pub C2_NS, "sched.idle.c2_ns");
    klog::counter!(pub C3_ENTRIES, "sched.idle.c3_entries");
    klog::counter!(pub C3_NS, "sched.idle.c3_ns");
}

/// The machine itself.
struct Ports;

/// PM1 status as `entry` needs it: a refused read counts as bus-master activity (fail closed, no C3).
pub fn bm_word<E>(r: Result<u16, E>) -> u16 {
    r.unwrap_or(BM_STS)
}

impl IdleHw for Ports {
    fn read8(&self, port: u16) -> bool {
        hal::portio::inb(port).is_ok()
    }
    fn read16(&self, port: u16) -> u16 {
        bm_word(hal::portio::inw(port))
    }
    fn write16(&self, port: u16, v: u16) {
        let _ = hal::portio::outw(port, v);
    }
    fn read32(&self, port: u16) {
        let _ = hal::portio::inl(port);
    }
    fn halt(&self) {
        hal::cpu::enable_and_halt();
    }
    fn sti(&self) {
        hal::cpu::enable_interrupts();
    }
}

/// An AP at bring-up: the plan was judged on the boot CPU's CPUID; a CPU that lacks ARAT or an invariant TSC
/// itself (a hybrid or buggy firmware) stays in C1, and says so once.
pub(crate) fn check_cpu(cs: &CpuSched) {
    let (arat, invariant_tsc) = CpuIdle::timer_bits_this_cpu();
    if arat && invariant_tsc {
        return;
    }
    cs.idle_c1_only.store(true, Ordering::Relaxed);
    if PLAN.get().is_some_and(|p| p.c1_only.is_none()) {
        klog::event!(Info, "sched", "idle.c1_only", reason = "cpu_mismatch", cpu = cs.cpu, arat = arat, invariant_tsc = invariant_tsc;
            "this CPU lacks what the boot CPU has; it stays in C1");
    }
}

/// One idle entry: the choice and the entry (generic: the idle loop runs it on the machine, the tests on the
/// recorder). `gap` (time to this CPU's next own timer) is asked only when a deep state is possible.
pub fn step<H: IdleHw>(hw: &H, plan: Option<&Plan>, m: Mode, cpu_c1_only: bool, gap: impl FnOnce() -> Option<u64>, predicted_ns: u64,
    factor: u32) -> Entered {
    let Some(p) = plan.filter(|p| !p.deep.is_empty() && m != Mode::C1 && !cpu_c1_only) else {
        hw.halt();
        return Entered::C1;
    };
    enter(hw, p, choose(m, &p.deep, gap(), predicted_ns, factor))
}

/// Sleeps this idle CPU until an interrupt (called with interrupts off, after the halt checks; returns with
/// interrupts on). No lock, no allocation before the sleep.
pub(crate) fn sleep(cs: &CpuSched) {
    let t0 = time::now_ns();
    let plan = PLAN.get();
    let gap = || match time::hrtimer::next_deadline_on(cs.cpu) {
        Some(None) => None,
        Some(Some(d)) => Some(d.saturating_sub(t0)),
        // Busy queue this instant (or none): assume a timer soon.
        None => Some(0),
    };
    let got = step(&Ports, plan, mode(), cs.idle_c1_only.load(Ordering::Relaxed), gap, cs.idle_predicted.load(Ordering::Relaxed),
        residency_factor());
    let slept = time::now_ns().saturating_sub(t0);
    let kind_of = |i: usize| plan.and_then(|p| p.deep.get(i)).map_or(1, |d| d.kind);
    #[cfg(bitsos_fd_measure)]
    {
        let kind = match got {
            Entered::Deep(i) | Entered::Demoted { to: Some(i), .. } => kind_of(i),
            _ => 1,
        };
        let (n, ns) = match kind {
            1 => (&measure::C1_ENTRIES, &measure::C1_NS),
            2 => (&measure::C2_ENTRIES, &measure::C2_NS),
            _ => (&measure::C3_ENTRIES, &measure::C3_NS),
        };
        n.inc();
        ns.add(slept);
    }
    match got {
        Entered::Demoted { from, to } => {
            BM_DEMOTIONS.inc();
            // Interrupts are on again: one event per boot, the counter for the rest.
            if !DEMOTION_LOGGED.swap(true, Ordering::Relaxed) {
                klog::event!(Info, "sched", "idle.bm_demote", cpu = cs.cpu, from_kind = kind_of(from), to_kind = to.map_or(1, kind_of);
                    "bus-master activity before C3: a shallower state (Linux does the same); counted in sched.idle.bm_demotions");
            }
        }
        // Really wrong (the plan admitted a port the hal guard refuses): once per boot, after the sti.
        Entered::Refused(i) if !REFUSAL_LOGGED.swap(true, Ordering::Relaxed) => {
            let port = plan.and_then(|p| p.deep.get(i)).map_or(0, |d| d.port);
            klog::event!(Warn, "sched", "idle.port_refused", cpu = cs.cpu, kind = kind_of(i), port = klog::Hex(port as u64);
                "the port guard refused a planned C-state port; C1 instead");
        }
        _ => {}
    }
    let prev = cs.idle_predicted.load(Ordering::Relaxed);
    cs.idle_predicted.store(predict(prev, slept), Ordering::Relaxed);
}

#[cfg(test)]
mod tests;
