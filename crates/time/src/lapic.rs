//! Every CPU's LAPIC timer as the clock of its hrtimer queue (spec/core.md §7.2; V1 `apic/timer.asm`).
//!
//! **TSC-deadline mode** where CPUID offers it (the interrupt comes when the TSC reaches the deadline: no
//! calibration, no rounding), else **one-shot mode** at divide 16, calibrated per CPU against the TSC (three
//! 1 ms windows, the middle result kept, as V1 did with 10 ms ones; the Mini-PC: ~1500 counts per ms, no
//! TSC-deadline, ARAT). Each CPU starts its own timer (`start_this_cpu`: the BSP from `acpi::irq::init`, the
//! APs through CP-4's cross-CPU call), and only then do timers armed on it leave the polled queue.
//!
//! CONFIG `lapic_timer=auto|oneshot|deadline` (auto: deadline where offered) and `hrtimer=interrupt|poll` (poll
//! keeps every timer on the polled queue: an escape hatch should a LAPIC timer misbehave on some machine).
//!
//! The interrupt (`on_timer`, system vector 0xF0) runs this CPU's due timers, then the scheduler's tick hook
//! (`sched::timer_tick`, installed by `platform`). The interrupt exit's preemption is hal's hook (CP-5).

use crate::hrtimer::{self, NO_DEADLINE, Queue};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use hal::apic::timer::{self as hw, TimerMode};
use hal::percpu::PerCpu;

pub const DIVIDE: u32 = 16;
/// One calibration window and how many (V1: three, the middle one kept).
pub const CAL_WINDOW_NS: u64 = 1_000_000;
pub const CAL_ROUNDS: usize = 3;
/// Plausible one-shot counts per ms at divide 16 (V1 `LAPIC_MS_MIN/MAX`: any bus clock seen in a PC).
pub const MIN_COUNTS_PER_MS: u64 = 100;
pub const MAX_COUNTS_PER_MS: u64 = 10_000_000;

/// CONFIG `lapic_timer=` / `hrtimer=`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Config {
    Auto,
    OneShot,
    Deadline,
    /// `hrtimer=poll`: no LAPIC timer at all.
    Poll,
}

impl Config {
    fn from_u8(v: u8) -> Config {
        match v {
            1 => Config::OneShot,
            2 => Config::Deadline,
            3 => Config::Poll,
            _ => Config::Auto,
        }
    }
    fn as_u8(self) -> u8 {
        match self {
            Config::Auto => 0,
            Config::OneShot => 1,
            Config::Deadline => 2,
            Config::Poll => 3,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Config::Auto => "auto",
            Config::OneShot => "oneshot",
            Config::Deadline => "deadline",
            Config::Poll => "poll",
        }
    }
}

/// The CONFIG values to a mode (pure; host-tested). Unknown values fall back to `Auto` and are named in the
/// second field so the caller can warn.
pub fn parse_config(lapic_timer: Option<&str>, hrtimer: Option<&str>) -> (Config, Option<&'static str>) {
    match hrtimer.map(str::trim) {
        Some("poll") => return (Config::Poll, None),
        None | Some("") | Some("interrupt") => {}
        Some(_) => return (parse_lapic(lapic_timer).0, Some("hrtimer")),
    }
    parse_lapic(lapic_timer)
}

fn parse_lapic(v: Option<&str>) -> (Config, Option<&'static str>) {
    match v.map(str::trim) {
        None | Some("") | Some("auto") => (Config::Auto, None),
        Some("oneshot") => (Config::OneShot, None),
        Some("deadline") => (Config::Deadline, None),
        Some(_) => (Config::Auto, Some("lapic_timer")),
    }
}

static CONFIG: AtomicU8 = AtomicU8::new(0);

pub(crate) fn set_config(c: Config) {
    CONFIG.store(c.as_u8(), Ordering::Release);
}

pub fn config() -> Config {
    Config::from_u8(CONFIG.load(Ordering::Acquire))
}

/// How a CPU's timer counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    OneShot,
    TscDeadline,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::OneShot => "oneshot",
            Kind::TscDeadline => "tsc_deadline",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartError {
    /// `hrtimer=poll`.
    Disabled,
    /// The LAPIC is not set up, or refused the mode.
    NoLapic,
    /// Vector 0xF0 belongs to someone else.
    Vector,
    /// The one-shot timer counted implausibly (counts per ms; 0: it never counted).
    Calibration(u64),
    /// No per-CPU state for this CPU (the queues were not prepared).
    NoState,
    /// No ARAT and an idle that would stop the timer (`usable`).
    NoArat,
}

impl StartError {
    pub fn name(self) -> &'static str {
        match self {
            StartError::Disabled => "disabled",
            StartError::NoLapic => "no_lapic",
            StartError::Vector => "vector_taken",
            StartError::Calibration(_) => "calibration",
            StartError::NoState => "no_state",
            StartError::NoArat => "no_arat",
        }
    }
    /// For a cross-CPU call's u64 result (`encode`/`decode`).
    fn code(self) -> u64 {
        match self {
            StartError::Disabled => 2,
            StartError::NoLapic => 3,
            StartError::Vector => 4,
            StartError::Calibration(_) => 5,
            StartError::NoState => 6,
            StartError::NoArat => 7,
        }
    }
}

/// A start result as one u64 (cross-CPU calls return one word): 0 one-shot, 1 TSC-deadline, else an error.
pub fn encode(r: Result<Kind, StartError>) -> u64 {
    match r {
        Ok(Kind::OneShot) => 0,
        Ok(Kind::TscDeadline) => 1,
        Err(e) => e.code(),
    }
}

/// The inverse of `encode` (a calibration error comes back with 0 counts; `cpu_stats` has the number).
pub fn decode(v: u64) -> Result<Kind, StartError> {
    match v {
        0 => Ok(Kind::OneShot),
        1 => Ok(Kind::TscDeadline),
        2 => Err(StartError::Disabled),
        3 => Err(StartError::NoLapic),
        4 => Err(StartError::Vector),
        5 => Err(StartError::Calibration(0)),
        7 => Err(StartError::NoArat),
        _ => Err(StartError::NoState),
    }
}

/// One CPU's timer state.
#[derive(Default)]
pub struct CpuTimer {
    live: AtomicBool,
    deadline: AtomicBool,
    /// One-shot counts per TSC tick, 32.32 fixed point.
    cnt_mult: AtomicU64,
    counts_per_ms: AtomicU64,
    irqs: AtomicU64,
    /// Interrupts that found no timer due (a cancelled or moved timer, a capped far deadline).
    early: AtomicU64,
    /// The last start error's code (0: none).
    error: AtomicU64,
    /// CPUID 6 EAX bit 2 (ARAT): the timer keeps running in every C-state.
    arat: AtomicBool,
}

static CPUS: PerCpu<CpuTimer> = PerCpu::new(|_| CpuTimer::default());
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// 0: not tried, 1: registered, 2: taken by someone else, 3: a CPU is registering it now.
static VECTOR: AtomicU8 = AtomicU8::new(0);
/// How long a CPU waits for another one's registration (TSC ticks; ~0.1 s at 1 GHz, a table write takes µs).
const VECTOR_WAIT_TICKS: u64 = 100_000_000;

/// Per-CPU numbers (ktests, MCP, the Mini-PC check list).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuTimerStats {
    pub live: bool,
    pub kind: Option<Kind>,
    pub counts_per_ms: u64,
    pub irqs: u64,
    pub early: u64,
    pub error: u64,
    /// The timer runs in every C-state (ARAT). Without it the timer is still used: BitsOS idles with `hlt`
    /// (C1), where the LAPIC timer always runs; only C3 and deeper stop it (SDM 11.5.4), which needs `mwait`.
    pub arat: bool,
}

pub fn cpu_stats(cpu: usize) -> Option<CpuTimerStats> {
    let c = CPUS.find_for(cpu)?;
    let live = c.live.load(Ordering::Acquire);
    Some(CpuTimerStats {
        live,
        kind: live.then(|| if c.deadline.load(Ordering::Acquire) { Kind::TscDeadline } else { Kind::OneShot }),
        counts_per_ms: c.counts_per_ms.load(Ordering::Relaxed),
        irqs: c.irqs.load(Ordering::Relaxed),
        early: c.early.load(Ordering::Relaxed),
        error: c.error.load(Ordering::Relaxed),
        arat: c.arat.load(Ordering::Relaxed),
    })
}

/// CPUs whose LAPIC timer drives their queue.
pub fn live_cpus() -> usize {
    LIVE.load(Ordering::Acquire)
}

pub(crate) fn live(cpu: usize) -> bool {
    CPUS.find_for(cpu).is_some_and(|c| c.live.load(Ordering::Acquire))
}

/// Per-CPU state for every possible CPU and the queues (thread context, BSP, before any AP starts).
pub(crate) fn prepare() {
    CPUS.cover_possible();
    hrtimer::prepare_queues();
}

/// Registers the timer's system vector once (any context). The first CPU claims the registration (0 to 3), the
/// others wait for its outcome (bounded: one table write), so two CPUs starting at once cannot both see
/// `Taken` and one of them give up on a vector that is in fact its own handler.
fn register_vector() -> bool {
    match VECTOR.compare_exchange(0, 3, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            let ok = hal::vectors::register_system(hal::vectors::LAPIC_TIMER, on_timer).is_ok();
            VECTOR.store(if ok { 1 } else { 2 }, Ordering::Release);
            ok
        }
        Err(_) => {
            let start = hal::cpu::rdtsc();
            loop {
                match VECTOR.load(Ordering::Acquire) {
                    1 => return true,
                    3 if hal::cpu::rdtsc().wrapping_sub(start) < VECTOR_WAIT_TICKS => hal::cpu::spin_hint(),
                    _ => return false,
                }
            }
        }
    }
}

/// Starts this CPU's timer: picks the mode, calibrates a one-shot timer, moves the polled timers here and
/// programs the earliest deadline. Interrupts off inside; no heap, no klog (an AP runs it from a cross-CPU
/// call in hard-IRQ context): the caller logs `cpu_stats`. A second call reports the running mode.
pub fn start_this_cpu() -> Result<Kind, StartError> {
    let r = start_inner();
    if let (Err(e), Some(c)) = (r, CPUS.find_for(hal::percpu::this_cpu())) {
        c.error.store(e.code(), Ordering::Relaxed);
    }
    r
}

fn start_inner() -> Result<Kind, StartError> {
    let cfg = config();
    if cfg == Config::Poll {
        return Err(StartError::Disabled);
    }
    let _irq = hal::cpu::IrqGuard::new();
    let cpu = hal::percpu::this_cpu();
    let c = CPUS.find_for(cpu).ok_or(StartError::NoState)?;
    if c.live.load(Ordering::Acquire) {
        return Ok(if c.deadline.load(Ordering::Acquire) { Kind::TscDeadline } else { Kind::OneShot });
    }
    if !register_vector() {
        return Err(StartError::Vector);
    }
    // No ARAT: noted (`log_cpu_start` warns) and, as long as idle is `hlt`, the timer is used anyway.
    let arat = hal::cpu::live().has("arat");
    c.arat.store(arat, Ordering::Relaxed);
    if !usable(arat, IDLE_USES_HLT_ONLY) {
        return Err(StartError::NoArat);
    }
    let offers_deadline = hal::cpu::live().has("tsc_deadline_timer");
    let use_deadline = offers_deadline && cfg != Config::OneShot;
    let kind = if use_deadline {
        hw::setup(TimerMode::TscDeadline, false).map_err(|_| StartError::NoLapic)?;
        c.deadline.store(true, Ordering::Release);
        Kind::TscDeadline
    } else {
        let (mult, per_ms) = calibrate()?;
        c.cnt_mult.store(mult, Ordering::Relaxed);
        c.counts_per_ms.store(per_ms, Ordering::Relaxed);
        hw::setup(TimerMode::OneShot { divide: DIVIDE }, false).map_err(|_| StartError::NoLapic)?;
        c.deadline.store(false, Ordering::Release);
        Kind::OneShot
    };
    c.live.store(true, Ordering::Release);
    LIVE.fetch_add(1, Ordering::AcqRel);
    hrtimer::with_cpu_queue(cpu, |q| {
        q.programmed = NO_DEADLINE;
        program(q, cpu);
    });
    hrtimer::adopt_polled();
    Ok(kind)
}

/// BitsOS idles with `hlt` only (sched's idle loop, C1): no `mwait` C-states.
pub const IDLE_USES_HLT_ONLY: bool = true;

/// Whether this CPU's LAPIC timer may drive its queue (pure; host-tested). With ARAT always; without it only
/// while idle stays in C1, where the LAPIC timer always runs (C3 and deeper stop it, SDM 11.5.4; Linux
/// `CLOCK_EVT_FEAT_C3STOP` then needs a broadcast timer). Otherwise the CPU's timers stay polled.
pub const fn usable(has_arat: bool, idle_hlt_only: bool) -> bool {
    has_arat || idle_hlt_only
}

/// (TSC right before the read, current count).
fn stamp() -> (u64, u32) {
    let t = hal::cpu::rdtsc_ordered();
    (t, hw::current_count())
}

/// Counts per TSC tick (32.32) and counts per ms of this CPU's one-shot timer at `DIVIDE`; the timer is
/// stopped and masked afterwards.
fn calibrate() -> Result<(u64, u64), StartError> {
    hw::setup(TimerMode::OneShot { divide: DIVIDE }, true).map_err(|_| StartError::NoLapic)?;
    let hz = crate::tsc_hz().max(crate::clock::MIN_HZ);
    let window = crate::clock::scale(CAL_WINDOW_NS, crate::clock::tick_mult(hz));
    let mut mults = [0u64; CAL_ROUNDS];
    for m in mults.iter_mut() {
        hw::arm_count(u32::MAX);
        let (t0, c0) = stamp();
        while hal::cpu::rdtsc().wrapping_sub(t0) < window {
            hal::cpu::spin_hint();
        }
        let (t1, c1) = stamp();
        // A timer that ran out (0) or never moved tells nothing.
        if c1 != 0 && c0 > c1 && t1 > t0 {
            *m = (((c0 - c1) as u128) << 32).checked_div((t1 - t0) as u128).unwrap_or(0).min(u64::MAX as u128) as u64;
        }
    }
    hw::arm_count(0);
    mults.sort_unstable();
    let mult = mults[CAL_ROUNDS / 2];
    let per_ms = crate::clock::scale(crate::clock::scale(1_000_000, crate::clock::tick_mult(hz)), mult);
    if !(MIN_COUNTS_PER_MS..=MAX_COUNTS_PER_MS).contains(&per_ms) {
        return Err(StartError::Calibration(per_ms));
    }
    Ok((mult, per_ms))
}

/// Programs `cpu`'s timer at the queue's earliest deadline (none: stopped). Only on `cpu` itself, with its
/// queue locked (interrupts off).
pub(crate) fn program(q: &mut Queue, cpu: usize) {
    let d = q.earliest().unwrap_or(NO_DEADLINE);
    program_at(q, cpu, d);
}

pub(crate) fn program_at(q: &mut Queue, cpu: usize, deadline: u64) {
    if cpu != hal::percpu::this_cpu() {
        return;
    }
    let Some(c) = CPUS.find_for(cpu).filter(|c| c.live.load(Ordering::Acquire)) else { return };
    if deadline == q.programmed {
        return;
    }
    q.programmed = deadline;
    let dl = c.deadline.load(Ordering::Relaxed);
    if deadline == NO_DEADLINE {
        if dl {
            // `c.deadline` is set only after `hw::setup(TscDeadline)` passed, so this is never refused.
            let _ = hw::arm_deadline(0);
        } else {
            hw::arm_count(0)
        }
        return;
    }
    let snap = crate::CLOCK.snapshot();
    if dl {
        let _ = hw::arm_deadline(snap.tsc_at(deadline).max(1));
    } else {
        let now = snap.ns_at(hal::cpu::rdtsc());
        let ticks = snap.ticks_in(deadline.saturating_sub(now));
        // Rounded up by one count: an interrupt a hair early only costs a second one.
        let count = crate::clock::scale(ticks, c.cnt_mult.load(Ordering::Relaxed)).saturating_add(1).min(u32::MAX as u64);
        hw::arm_count(count as u32);
    }
}

/// The LAPIC timer interrupt (vector 0xF0, every CPU; hard-IRQ context).
fn on_timer(_v: u8) {
    let cpu = hal::percpu::this_cpu();
    if let Some(c) = CPUS.find_for(cpu) {
        c.irqs.fetch_add(1, Ordering::Relaxed);
        if hrtimer::run_cpu(cpu) == 0 {
            c.early.fetch_add(1, Ordering::Relaxed);
        }
    }
    if let Some(f) = crate::TICK_HOOK.get() {
        f();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values() {
        assert_eq!(parse_config(None, None), (Config::Auto, None));
        assert_eq!(parse_config(Some("oneshot"), None), (Config::OneShot, None));
        assert_eq!(parse_config(Some(" deadline "), Some("interrupt")), (Config::Deadline, None));
        assert_eq!(parse_config(Some("deadline"), Some("poll")), (Config::Poll, None), "poll wins");
        assert_eq!(parse_config(Some("fast"), None), (Config::Auto, Some("lapic_timer")));
        assert_eq!(parse_config(None, Some("sometimes")), (Config::Auto, Some("hrtimer")));
    }

    #[test]
    fn start_codes() {
        assert_eq!(encode(Ok(Kind::OneShot)), 0);
        assert_eq!(encode(Ok(Kind::TscDeadline)), 1);
        assert_eq!(encode(Err(StartError::Calibration(5))), 5);
        for r in [Ok(Kind::OneShot), Ok(Kind::TscDeadline), Err(StartError::Disabled), Err(StartError::Vector), Err(StartError::NoState)] {
            assert_eq!(decode(encode(r)), r);
        }
        assert_eq!(decode(encode(Err(StartError::Calibration(77)))), Err(StartError::Calibration(0)));
    }

    #[test]
    fn arat_decision() {
        assert!(usable(true, false) && usable(true, true));
        assert!(usable(false, IDLE_USES_HLT_ONLY), "hlt idle (C1) keeps every LAPIC timer running");
        assert!(!usable(false, false), "deep C-states without ARAT: stay polled");
        assert_eq!(decode(encode(Err(StartError::NoArat))), Err(StartError::NoArat));
    }

    #[test]
    fn the_host_has_no_lapic_timer() {
        prepare();
        assert_eq!(start_this_cpu(), Err(StartError::NoLapic));
        assert!(!live(0));
        assert_eq!(cpu_stats(0).unwrap().error, 3);
    }
}
