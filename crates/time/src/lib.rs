//! Time (owner: CP; contract `C: HrTimer`, V2-PLAN §2.4, spec/core.md §7). CP-5:
//!
//! - **Clock** (`clock`): the TSC behind `now_ns`. `init` (kernel start) takes the rate a Reload handed over, or
//!   a quick 10 ms PIT measurement; `calibrate` (CP's `acpi::init`, once the tables are read) starts the HPET and
//!   measures the TSC against it over ~100 ms, else against the ACPI PM timer, else the PIT (V1 `time.asm`
//!   order; `calib`, `sources`), checks the loader's rate (2 %), CPUID 0x15/0x16 and the invariant-TSC bit, and
//!   rebases the clock so it never jumps back. A Reload keeps the rate it was handed (spec §7.1), and checks it
//!   against the HPET over one second without waiting for it (`recheck`, INT-A1e): a rate more than 50 ppm off
//!   is replaced (rebased, never back), logged as `time tsc.rehandover`, and handed on at the next Reload.
//! - **HrTimer** (`hrtimer`, `lapic`): per-CPU queues, each programmed into its CPU's LAPIC timer (TSC-deadline
//!   or calibrated one-shot); callbacks run from the timer interrupt; the polled queue remains for CPUs whose
//!   timer is not running (and `hrtimer=poll`). The interrupt also calls the scheduler's tick hook
//!   (`set_tick_hook`); `interrupt_driven()` tells the scheduler when no CPU has to poll for timers.
//! - **Wall clock** (`wall`, `rtc`, `tz`): the RTC's local time to UTC with CONFIG `tz=` (EU rule by default).
//!
//! Who calls what: the kernel `init`; CP's `acpi::init` → `calibrate` + `wall::read_rtc`; CP's `acpi::irq::init`
//! → `configure` + `start_bsp`; CP's `platform` (`cpu.timers`) → `lapic::start_this_cpu` on every AP and the
//! hooks.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod calib;
pub mod clock;
mod heap;
pub mod hrtimer;
pub mod lapic;
pub mod recheck;
pub mod rtc;
pub mod sources;
pub mod tz;
pub mod wall;

pub use hrtimer::{HrTimer, next_deadline, poll_timers};

use alloc::string::String;
use alloc::vec::Vec;
use bootinfo::tsc_source;
use calib::CalibError;
use clock::Clock;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use ksync::{Once, SpinLock};
use sources::{CpuidRates, Hpet, HpetError, HpetInfo, PmTimer};

pub(crate) static CLOCK: Clock = Clock::new();
pub(crate) static TICK_HOOK: Once<fn()> = Once::new();
static SOURCE: AtomicU32 = AtomicU32::new(tsc_source::NONE);
static HPET: Once<Hpet> = Once::new();
static CALIBRATION: SpinLock<Option<Calibration>> = SpinLock::named("time.calibration", None);
/// The first sample of the boot's re-check of a handed-on rate (taken by `calibrate`, used by `finish_recheck`).
static RECHECK_START: SpinLock<Option<recheck::Sample>> = SpinLock::named("time.recheck", None);
static RECHECK_PENDING: AtomicBool = AtomicBool::new(false);

/// The rate when nothing could be measured (V1 `TSC_GUESS`).
pub const GUESS_HZ: u64 = 2_000_000_000;

/// First clock (kernel start, before the ACPI tables): the rate a Reload handed over, else 10 ms of PIT.
pub fn init(reload_calibrated_hz: u64) {
    let t = hal::cpu::rdtsc();
    let (hz, src, name) = if reload_calibrated_hz != 0 {
        (reload_calibrated_hz, tsc_source::PIT, "reload")
    } else {
        match sources::calibrate_with_pit(10) {
            Some(hz) if (clock::MIN_HZ..=clock::MAX_HZ).contains(&hz) => (hz, tsc_source::PIT, "pit"),
            _ => (GUESS_HZ, tsc_source::NONE, "guess"),
        }
    };
    CLOCK.set_rate(t, hz);
    SOURCE.store(src, Ordering::Release);
    klog::install_clock(now_ns);
    klog::event!(Info, "time", "clock.ready", tsc_hz = hz, source = name; "first rate; CP-5 calibrates against the HPET once the ACPI tables are read");
}

/// TSC ticks per second (0 before `init`).
pub fn tsc_hz() -> u64 {
    CLOCK.hz()
}

/// Monotonic nanoseconds since `init` (0 before). Lock-free; any context, NMI included.
pub fn now_ns() -> u64 {
    CLOCK.snapshot().ns_at(hal::cpu::rdtsc())
}

/// The monotonic ns a TSC value stands for (traces, the Reload hand-over).
pub fn ns_at_tsc(tsc: u64) -> u64 {
    CLOCK.snapshot().ns_at(tsc)
}

/// The calibration source as BootInfo's `tsc_source` code (kexec writes it into the next kernel's TSC record).
pub fn source() -> u32 {
    SOURCE.load(Ordering::Acquire)
}

/// Busy-waits (early boot, drivers' short register delays). Long waits must sleep instead (R5).
pub fn delay_ns(ns: u64) {
    let until = now_ns().saturating_add(ns);
    while now_ns() < until {
        hal::cpu::spin_hint();
    }
}

pub fn delay_us(us: u64) {
    delay_ns(us * 1000)
}

/// The scheduler's tick hook (`sched::timer_tick`), called at the end of every LAPIC timer interrupt. First call
/// wins (false after).
pub fn set_tick_hook(f: fn()) -> bool {
    let mut set = false;
    TICK_HOOK.get_or_init(|| {
        set = true;
        f
    });
    set
}

/// True when every online CPU's timers fire by its LAPIC timer interrupt: nobody has to poll for hrtimers
/// (the scheduler's poll-duty CPU may halt then). False with `hrtimer=poll`, while a CPU's timer is not live,
/// and while the polled queue holds timers (an arm that found no room in its CPU's queue, see `hrtimer`).
pub fn interrupt_driven() -> bool {
    let live = lapic::live_cpus();
    lapic::config() != lapic::Config::Poll && live > 0 && live >= hal::percpu::cpu_count() && hrtimer::polled_len() == 0
}

// ---------------------------------------------------------------------------------------------------------
// Calibration

/// What a TSC rate was taken from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Hpet,
    PmTimer,
    Pit,
    Cpuid15,
    LoaderHint,
    /// The rate the previous kernel handed over (with its original source code).
    Reload(u32),
    Guess,
}

impl Source {
    pub fn code(self) -> u32 {
        match self {
            Source::Hpet => tsc_source::HPET,
            Source::PmTimer => tsc_source::PM_TIMER,
            Source::Pit => tsc_source::PIT,
            Source::Cpuid15 => tsc_source::CPUID,
            Source::LoaderHint => tsc_source::UEFI_STALL,
            Source::Reload(c) => c,
            Source::Guess => tsc_source::NONE,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Source::Hpet => "hpet",
            Source::PmTimer => "pm_timer",
            Source::Pit => "pit",
            Source::Cpuid15 => "cpuid15",
            Source::LoaderHint => "loader",
            Source::Reload(_) => "reload",
            Source::Guess => "guess",
        }
    }
}

/// What `calibrate` is given (CP's `acpi` side, after the register guard allowed the addresses).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CalibInputs {
    pub hpet_phys: Option<u64>,
    pub pm_timer: Option<PmTimer>,
    /// The loader's rough rate (a plausibility bound).
    pub hint_hz: u64,
    /// A Reload's rate and its original source code (kept, not re-measured).
    pub reload: Option<(u64, u32)>,
}

/// The calibration's report (boot event, ktests, `hw_info`).
#[derive(Clone, Debug)]
pub struct Calibration {
    pub hz: u64,
    pub source: Source,
    pub first_hz: u64,
    pub hint_hz: u64,
    pub hpet: Option<HpetInfo>,
    pub hpet_error: Option<HpetError>,
    /// Each measurement tried, in order, with its result.
    pub tried: Vec<(Source, Result<u64, CalibError>)>,
    pub cpuid: CpuidRates,
    pub invariant_tsc: bool,
    pub took_ns: u64,
    /// The last re-check of a handed-on rate against the HPET (`None` before it finished, or after a cold boot).
    pub recheck: Option<recheck::Recheck>,
}

impl Calibration {
    /// ppm of the chosen rate against CPUID 0x15's exact one, where the CPU states it.
    pub fn ppm_vs_cpuid(&self) -> Option<i64> {
        self.cpuid.leaf15_hz.map(|c| calib::ppm(self.hz, c))
    }
}

/// The measurement order of spec §7.1 (pure apart from the closures; host-tested): a Reload's rate is kept;
/// otherwise the first of HPET, PM timer, PIT whose rate agrees with the loader's hint, then CPUID 0x15, the
/// hint itself, and the first rate as the last resort. Each measurement tried is recorded.
pub fn choose(
    inputs: &CalibInputs,
    hpet: Option<&mut dyn FnMut() -> Result<u64, CalibError>>,
    pm: Option<&mut dyn FnMut() -> Result<u64, CalibError>>,
    pit: &mut dyn FnMut() -> Result<u64, CalibError>,
    cpuid15: Option<u64>,
    first_hz: u64,
    tried: &mut Vec<(Source, Result<u64, CalibError>)>,
) -> (u64, Source) {
    if let Some((hz, code)) = inputs.reload.filter(|r| (clock::MIN_HZ..=clock::MAX_HZ).contains(&r.0)) {
        return (hz, Source::Reload(code));
    }
    let try_one = |src: Source, f: &mut dyn FnMut() -> Result<u64, CalibError>, tried: &mut Vec<_>| {
        let r = f();
        tried.push((src, r));
        r.ok().filter(|&hz| calib::agrees_with_hint(hz, inputs.hint_hz))
    };
    if let Some(f) = hpet
        && let Some(hz) = try_one(Source::Hpet, f, tried)
    {
        return (hz, Source::Hpet);
    }
    if let Some(f) = pm
        && let Some(hz) = try_one(Source::PmTimer, f, tried)
    {
        return (hz, Source::PmTimer);
    }
    if let Some(hz) = try_one(Source::Pit, pit, tried) {
        return (hz, Source::Pit);
    }
    if let Some(hz) = cpuid15.filter(|&hz| calib::agrees_with_hint(hz, inputs.hint_hz)) {
        return (hz, Source::Cpuid15);
    }
    if (clock::MIN_HZ..=clock::MAX_HZ).contains(&inputs.hint_hz) {
        return (inputs.hint_hz, Source::LoaderHint);
    }
    (if first_hz != 0 { first_hz } else { GUESS_HZ }, Source::Guess)
}

/// Starts the HPET and calibrates the TSC (once; later calls return the first report). Boot, interrupts as they
/// are; ~100 ms per source tried.
pub fn calibrate(inputs: &CalibInputs) -> Calibration {
    if let Some(c) = CALIBRATION.lock().clone() {
        return c;
    }
    let t0 = now_ns();
    let first_hz = tsc_hz();
    let (hpet, hpet_error) = match inputs.hpet_phys.map(Hpet::start) {
        Some(Ok(h)) => {
            let info = h.info;
            HPET.get_or_init(|| h);
            (Some(info), None)
        }
        Some(Err(e)) => (None, Some(e)),
        None => (None, None),
    };
    let cpuid = sources::cpuid_rates();
    let mut tried = Vec::new();
    let mut hpet_f = || match HPET.get() {
        Some(h) if h.runs() => h.calibrate(),
        _ => Err(CalibError::Stalled),
    };
    let mut pm_f = || inputs.pm_timer.map_or(Err(CalibError::Stalled), |p| p.calibrate());
    let mut pit_f = || sources::calibrate_with_pit(50).ok_or(CalibError::Stalled);
    let chosen = choose(
        inputs,
        hpet.is_some().then_some(&mut hpet_f as &mut dyn FnMut() -> Result<u64, CalibError>),
        inputs.pm_timer.is_some().then_some(&mut pm_f as &mut dyn FnMut() -> Result<u64, CalibError>),
        &mut pit_f,
        cpuid.leaf15_hz,
        first_hz,
        &mut tried,
    );
    CLOCK.set_rate(hal::cpu::rdtsc(), chosen.0);
    SOURCE.store(chosen.1.code(), Ordering::Release);
    let c = Calibration {
        hz: chosen.0,
        source: chosen.1,
        first_hz,
        hint_hz: inputs.hint_hz,
        hpet,
        hpet_error,
        tried,
        cpuid,
        invariant_tsc: hal::cpu::live().has("constant_tsc"),
        took_ns: now_ns().saturating_sub(t0),
        recheck: None,
    };
    report(&c);
    *CALIBRATION.lock() = Some(c.clone());
    // A handed-on rate is checked against the HPET (INT-A1e): the first sample now, the second one second later
    // from a thread (`recheck_due_ns`, `finish_recheck`), so a Reload starts no slower.
    if matches!(c.source, Source::Reload(_))
        && let Some(h) = HPET.get().filter(|h| h.runs())
    {
        *RECHECK_START.lock() = Some(hpet_sample(h));
        RECHECK_PENDING.store(true, Ordering::Release);
    }
    c
}

// ---------------------------------------------------------------------------------------------------------
// Re-checking a handed-on rate (INT-A1e)

/// (TSC, HPET) read together: of 8 tries with interrupts off the one whose two TSC reads lie closest, the TSC
/// taken at their middle (`calib::sample`'s rule).
fn hpet_sample(h: &Hpet) -> recheck::Sample {
    let _irq = hal::cpu::IrqGuard::new();
    let mut best = (u64::MAX, recheck::Sample { tsc: 0, count: 0 });
    for _ in 0..8 {
        let t0 = hal::cpu::rdtsc_ordered();
        let count = h.read();
        let t1 = hal::cpu::rdtsc_ordered();
        let d = t1.wrapping_sub(t0);
        if d < best.0 {
            best = (d, recheck::Sample { tsc: t0 + d / 2, count });
        }
    }
    best.1
}

fn under_hypervisor() -> bool {
    !hal::BARE_METAL || hal::cpu::live().has("hypervisor")
}

/// The monotonic ns at which the boot's re-check can finish (`None`: nothing to re-check, or done).
pub fn recheck_due_ns() -> Option<u64> {
    if !RECHECK_PENDING.load(Ordering::Acquire) {
        return None;
    }
    RECHECK_START.lock().map(|s| ns_at_tsc(s.tsc).saturating_add(recheck::SPAN_NS))
}

/// Whether the boot's re-check is still to come (ktests wait for it before they compare the clock).
pub fn recheck_pending() -> bool {
    RECHECK_PENDING.load(Ordering::Acquire)
}

/// Finishes the boot's re-check (CP's `time.recheck` thread, at `recheck_due_ns`): measures the rate since the
/// first sample, judges it (a hypervisor only logs), corrects the clock if needed and logs. `None` when nothing
/// was pending.
pub fn finish_recheck() -> Option<recheck::Recheck> {
    let start = RECHECK_START.lock().take()?;
    let r = conclude(start, under_hypervisor(), true);
    RECHECK_PENDING.store(false, Ordering::Release);
    Some(r)
}

/// A re-check over `window_ns` now (ktests): `sleep` waits, `as_hardware` judges as on bare metal even under a
/// hypervisor (a ktest that fakes a wrong rate wants the correction). It corrects and logs like the boot's, but
/// `Calibration::recheck` keeps the boot's result. `None` without a running HPET.
pub fn recheck_over(window_ns: u64, sleep: &dyn Fn(u64), as_hardware: bool) -> Option<recheck::Recheck> {
    let h = HPET.get().filter(|h| h.runs())?;
    let start = hpet_sample(h);
    sleep(window_ns);
    Some(conclude(start, under_hypervisor() && !as_hardware, false))
}

fn conclude(start: recheck::Sample, hypervisor: bool, record: bool) -> recheck::Recheck {
    let handed = tsc_hz();
    let hint = CALIBRATION.lock().as_ref().map_or(0, |c| c.hint_hz);
    let (measured, end_tsc) = match HPET.get() {
        Some(h) => {
            let end = hpet_sample(h);
            (recheck::rate_between(start, end, h.counter(), handed), end.tsc)
        }
        None => (Err(CalibError::Stalled), start.tsc),
    };
    let (outcome, ppm) = recheck::judge(handed, measured, hint, hypervisor);
    let r = recheck::Recheck {
        handed_hz: handed,
        measured_hz: measured.unwrap_or(0),
        ppm,
        span_ns: (end_tsc.saturating_sub(start.tsc) as u128 * 1_000_000_000 / handed.max(1) as u128) as u64,
        outcome,
    };
    if outcome == recheck::Outcome::Corrected {
        // From now on (a rebase: what the clock reads now stays, it never goes back).
        apply_rate(hal::cpu::rdtsc(), r.measured_hz, Source::Hpet);
    }
    if record && let Some(c) = CALIBRATION.lock().as_mut() {
        c.recheck = Some(r);
    }
    let error = match outcome {
        recheck::Outcome::Failed(e) => alloc::format!("{e:?}"),
        _ => String::new(),
    };
    klog::event!(Info, "time", "tsc.recheck", outcome = outcome.name(), handed_hz = r.handed_hz, measured_hz = r.measured_hz,
        ppm = r.ppm, span_ms = r.span_ns / 1_000_000, hypervisor = hypervisor, error = error);
    match outcome {
        recheck::Outcome::Corrected => klog::event!(Warn, "time", "tsc.rehandover", old_hz = r.handed_hz, new_hz = r.measured_hz, ppm = r.ppm;
            "the rate a Reload handed over was off against the HPET: the clock counts on at the measured rate (no step back), and the next Reload hands that one on"),
        recheck::Outcome::Implausible if !hypervisor => klog::event!(Warn, "time", "tsc.recheck_implausible", handed_hz = r.handed_hz, measured_hz = r.measured_hz, ppm = r.ppm;
            "the HPET measurement is too far off to believe; the handed-on rate stays"),
        _ => {}
    }
    r
}

/// Sets the clock's rate from TSC value `tsc` on (rebased: it never goes back) and makes the source and the
/// calibration report say so.
fn apply_rate(tsc: u64, hz: u64, source: Source) {
    CLOCK.set_rate(tsc, hz);
    SOURCE.store(source.code(), Ordering::Release);
    if let Some(c) = CALIBRATION.lock().as_mut() {
        c.hz = hz;
        c.source = source;
    }
}

/// ktests only: runs the clock at `hz` from now on (rebased, never back) with `source` in the report, e.g. a
/// wrong handed-on rate for `recheck_over`, and the right one put back after it.
pub fn set_rate_for_test(hz: u64, source: Source) {
    if (clock::MIN_HZ..=clock::MAX_HZ).contains(&hz) {
        apply_rate(hal::cpu::rdtsc(), hz, source);
    }
}

/// Whether this kernel measured its rate itself (a cold boot's calibration, or a corrected handed-on rate), so
/// the next Reload records it as measured here (kexec's `next_tsc`).
pub fn rate_measured_here() -> bool {
    calibration().is_some_and(|c| !matches!(c.source, Source::Reload(_)))
}

fn report(c: &Calibration) {
    let tried: Vec<String> = c
        .tried
        .iter()
        .map(|(s, r)| match r {
            Ok(hz) => alloc::format!("{}={}", s.name(), hz),
            Err(e) => alloc::format!("{}={:?}", s.name(), e),
        })
        .collect();
    if let Some(h) = &c.hpet {
        klog::event!(Info, "time", "hpet.started", phys = klog::Hex(h.phys), hz = h.hz(), comparators = h.comparators,
            counter_64bit = h.counter_64bit, was_running = h.was_running, legacy_was_on = h.legacy_was_on);
    }
    if let Some(e) = c.hpet_error {
        klog::event!(Warn, "time", "hpet.unusable", error = alloc::format!("{e:?}"); "calibrating against the PM timer or the PIT instead");
    }
    klog::event!(Info, "time", "clock.calibrated", tsc_hz = c.hz, source = c.source.name(), first_hz = c.first_hz,
        ppm_vs_first = calib::ppm(c.hz, c.first_hz), hint_hz = c.hint_hz, ppm_vs_hint = calib::ppm(c.hz, c.hint_hz),
        cpuid15_hz = c.cpuid.leaf15_hz.unwrap_or(0), cpuid16_mhz = c.cpuid.leaf16_mhz.unwrap_or(0),
        invariant = c.invariant_tsc, tried = tried.join(","), took_ms = c.took_ns / 1_000_000);
    let hardware = hal::BARE_METAL && !hal::cpu::live().has("hypervisor");
    if hardware && !matches!(c.source, Source::Hpet | Source::Reload(_)) {
        klog::event!(Warn, "time", "clock.fallback", source = c.source.name(); "the HPET did not give the rate (spec §7.1 order: HPET, PM timer, PIT)");
    }
    if !c.invariant_tsc {
        if hardware {
            klog::event!(Warn, "time", "tsc.not_invariant"; "CPUID 0x80000007 EDX[8] is clear: the TSC may change rate in deep sleep states");
        } else {
            klog::event!(Info, "time", "tsc.not_invariant"; "the hypervisor does not say the TSC is invariant");
        }
    }
}

/// The calibration report (`None` before `calibrate`).
pub fn calibration() -> Option<Calibration> {
    CALIBRATION.lock().clone()
}

/// The started HPET's low 32 counter bits and its period in fs (`None` without an HPET). For checks of the
/// clock against it (ktests).
pub fn hpet_read() -> Option<(u64, u64)> {
    HPET.get().map(|h| (h.read(), h.info.fs_per_count as u64))
}

// ---------------------------------------------------------------------------------------------------------
// Configuration and the boot CPU's timer

/// CONFIG keys (CP's `acpi::irq::init` hands its getter over): `tz=`, `lapic_timer=`, `hrtimer=`.
pub fn configure(get: &dyn Fn(&str) -> Option<String>) {
    let (cfg, bad) = lapic::parse_config(get("lapic_timer").as_deref(), get("hrtimer").as_deref());
    lapic::set_config(cfg);
    if let Some(key) = bad {
        klog::event!(Warn, "time", "config.unknown", key = key, value = get(key).unwrap_or_default(); "lapic_timer= takes auto|oneshot|deadline, hrtimer= interrupt|poll");
    }
    if let Some(v) = get("tz") {
        match tz::parse(&v) {
            Some(z) => wall::set_tz(z),
            None => klog::event!(Warn, "time", "tz.unknown", value = v; "kept Europe/Berlin (EU summer time)"),
        }
    }
    let (base, z, named, err) = wall::state();
    let local = match base {
        wall::Base::Rtc { local, .. } => fmt_dt(&local, ""),
        _ => String::from("none"),
    };
    let utc = wall::utc_now().map(|d| fmt_dt(&d, "Z")).unwrap_or_default();
    klog::event!(Info, "time", "wall.ready", rtc_local = local, utc = utc, tz = z.describe(), tz_from_config = named,
        offset_min = wall::utc_offset_minutes(), rtc_error = err.map(|e| e.name()).unwrap_or(""));
}

/// `YYYY-MM-DDTHH:MM:SS` plus `suffix`.
pub fn fmt_dt(d: &tz::DateTime, suffix: &str) -> String {
    alloc::format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}{suffix}", d.year, d.month, d.day, d.hour, d.min, d.sec)
}

/// Prepares every CPU's timer state and queue (allocates) and starts the boot CPU's LAPIC timer (CP's
/// `acpi::irq::init`, after the LAPIC set-up). Logs the result.
pub fn start_bsp() -> Result<lapic::Kind, lapic::StartError> {
    lapic::prepare();
    let r = lapic::start_this_cpu();
    log_cpu_start(hal::percpu::this_cpu(), r);
    r
}

/// The event for one CPU's timer start (thread context).
pub fn log_cpu_start(cpu: usize, r: Result<lapic::Kind, lapic::StartError>) {
    let st = lapic::cpu_stats(cpu).unwrap_or_default();
    if !st.arat && matches!(r, Ok(_) | Err(lapic::StartError::NoArat)) {
        klog::event!(Warn, "time", "lapic.no_arat", cpu = cpu, idle_hlt_only = lapic::IDLE_USES_HLT_ONLY;
            "CPUID 6 has no ARAT: the LAPIC timer stops in C3 and deeper; idle is hlt (C1), so the timer is used");
    }
    match r {
        Ok(k) => klog::event!(Info, "time", "lapic.timer", cpu = cpu, mode = k.name(), counts_per_ms = st.counts_per_ms, config = lapic::config().name()),
        Err(lapic::StartError::Disabled) => klog::event!(Info, "time", "lapic.timer_off", cpu = cpu; "hrtimer=poll: timers are polled"),
        Err(e) => klog::event!(Warn, "time", "lapic.timer_failed", cpu = cpu, error = e.name(),
            counts_per_ms = match e { lapic::StartError::Calibration(n) => n, _ => 0 }; "this CPU's timers go to the polled queue"),
    }
}

/// Lines for MCP `hw_info` (section interrupts): the clock, the wall clock, every CPU's timer and the queues.
pub fn summary() -> Vec<String> {
    let mut out = Vec::new();
    match calibration() {
        Some(c) => out.push(alloc::format!(
            "time clock tsc_hz={} source={} ppm_vs_hint={} cpuid15_hz={} cpuid16_mhz={} invariant={} hpet={} now_ms={}",
            c.hz, c.source.name(), calib::ppm(c.hz, c.hint_hz), c.cpuid.leaf15_hz.unwrap_or(0), c.cpuid.leaf16_mhz.unwrap_or(0), c.invariant_tsc,
            c.hpet.map(|h| alloc::format!("{:#x}@{}Hz", h.phys, h.hz())).unwrap_or_else(|| String::from("none")), now_ns() / 1_000_000
        )),
        None => out.push(alloc::format!("time clock tsc_hz={} source=boot (not calibrated)", tsc_hz())),
    }
    let utc = wall::utc_now().map(|d| fmt_dt(&d, "Z")).unwrap_or_else(|| String::from("unknown"));
    out.push(alloc::format!("time wall utc={utc} offset_min={} tz={}", wall::utc_offset_minutes(), wall::tz().describe()));
    out.push(alloc::format!(
        "time timers config={} live={}/{} interrupt_driven={} hrtimers={} polled_queue={}",
        lapic::config().name(), lapic::live_cpus(), hal::percpu::cpu_count(), interrupt_driven(), hrtimer::count(),
        hrtimer::queue_state(None).map(|q| q.0).unwrap_or(0)
    ));
    for cpu in 0..hal::percpu::cpu_count() {
        let s = lapic::cpu_stats(cpu).unwrap_or_default();
        let queued = hrtimer::queue_state(Some(cpu)).map(|q| q.0).unwrap_or(0);
        out.push(alloc::format!(
            "time lapic cpu={cpu} mode={} counts_per_ms={} irqs={} early={} queued={queued} error={} arat={}",
            s.kind.map(|k| k.name()).unwrap_or("off"), s.counts_per_ms, s.irqs, s.early, s.error, s.arat
        ));
    }
    out
}

kreg::initcall!(kreg::Level::Core, "time.poller", register_poller);
fn register_poller() -> Result<(), &'static str> {
    hal::vectors::register_poller("time.timers", || {
        poll_timers();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(hz: u64) -> impl FnMut() -> Result<u64, CalibError> {
        move || Ok(hz)
    }
    fn bad() -> impl FnMut() -> Result<u64, CalibError> {
        || Err(CalibError::Stalled)
    }

    #[test]
    fn spec_order_hpet_pm_pit() {
        let i = CalibInputs { hint_hz: 3_790_000_000, ..Default::default() };
        let mut t = Vec::new();
        let (mut h, mut p, mut q) = (ok(3_792_877_000), ok(3_792_865_000), ok(3_792_938_000));
        assert_eq!(choose(&i, Some(&mut h), Some(&mut p), &mut q, None, 0, &mut t), (3_792_877_000, Source::Hpet));
        assert_eq!(t.len(), 1, "stops at the first good one");
        let (mut h, mut q) = (bad(), ok(3_792_938_000));
        let mut t = Vec::new();
        assert_eq!(choose(&i, Some(&mut h), Some(&mut p), &mut q, None, 0, &mut t).1, Source::PmTimer);
        // A rate more than 2 % off the loader's falls through to the next source.
        let (mut h, mut p) = (ok(3_000_000_000), bad());
        let mut t = Vec::new();
        assert_eq!(choose(&i, Some(&mut h), Some(&mut p), &mut q, None, 0, &mut t), (3_792_938_000, Source::Pit));
        assert_eq!(t.len(), 3);
    }

    #[test]
    fn last_resorts_and_reload() {
        let i = CalibInputs { hint_hz: 3_790_000_000, ..Default::default() };
        let mut t = Vec::new();
        let mut q = bad();
        assert_eq!(choose(&i, None, None, &mut q, Some(3_800_000_000), 0, &mut t).1, Source::Cpuid15);
        assert_eq!(choose(&i, None, None, &mut q, None, 0, &mut t), (3_790_000_000, Source::LoaderHint));
        let none = CalibInputs::default();
        assert_eq!(choose(&none, None, None, &mut q, None, 3_000_000_000, &mut t), (3_000_000_000, Source::Guess));
        assert_eq!(choose(&none, None, None, &mut q, None, 0, &mut t), (GUESS_HZ, Source::Guess));
        let r = CalibInputs { reload: Some((3_792_877_000, tsc_source::HPET)), ..Default::default() };
        let mut t = Vec::new();
        let mut h = ok(1);
        assert_eq!(choose(&r, Some(&mut h), None, &mut q, None, 0, &mut t), (3_792_877_000, Source::Reload(tsc_source::HPET)));
        assert!(t.is_empty(), "a Reload measures nothing");
        assert_eq!(Source::Reload(tsc_source::HPET).code(), tsc_source::HPET);
        assert_eq!(Source::Hpet.code(), 2);
    }
}
