//! Hold/wait statistics per lock class and the text of the `locks` MCP tool.
//!
//! Counted for every acquisition (release builds too): taken, contended (had to wait), total and longest wait,
//! total and longest hold, failed tries, timeouts. Times are TSC ticks, shown in µs once the scheduler gave
//! the TSC frequency (`set_tsc_hz`). Counters are per class, not per lock (one line per name).

use crate::class::{self, LockClass};
use crate::order;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

static TSC_HZ: AtomicU64 = AtomicU64::new(0);

/// The TSC frequency, for turning ticks into time (sched::init).
pub fn set_tsc_hz(hz: u64) {
    TSC_HZ.store(hz, Ordering::Release);
}

pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Acquire)
}

/// Ticks in ns (None before `set_tsc_hz`).
pub fn ticks_to_ns(t: u64) -> Option<u64> {
    let hz = tsc_hz();
    (hz != 0).then(|| ((t as u128 * 1_000_000_000) / hz as u128).min(u64::MAX as u128) as u64)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassStats {
    pub name: &'static str,
    pub level: u16,
    pub acquired: u64,
    pub contended: u64,
    pub wait_ticks: u64,
    pub wait_max_ticks: u64,
    pub hold_ticks: u64,
    pub hold_max_ticks: u64,
    pub try_failed: u64,
    pub timeouts: u64,
}

fn snap(c: &'static LockClass) -> ClassStats {
    let s = &c.stats;
    ClassStats {
        name: c.name,
        level: c.level,
        acquired: s.acquired.load(Ordering::Relaxed),
        contended: s.contended.load(Ordering::Relaxed),
        wait_ticks: s.wait_ticks.load(Ordering::Relaxed),
        wait_max_ticks: s.wait_max.load(Ordering::Relaxed),
        hold_ticks: s.hold_ticks.load(Ordering::Relaxed),
        hold_max_ticks: s.hold_max.load(Ordering::Relaxed),
        try_failed: s.try_failed.load(Ordering::Relaxed),
        timeouts: s.timeouts.load(Ordering::Relaxed),
    }
}

/// Every class with its counters (allocates).
pub fn classes() -> Vec<ClassStats> {
    let mut v = Vec::new();
    class::for_each(|c| v.push(snap(c)));
    v
}

/// The counters of one class by name (None if no lock of that name was ever taken or named in the table).
pub fn class(name: &str) -> Option<ClassStats> {
    let mut r = None;
    class::for_each(|c| {
        if r.is_none() && c.name == name {
            r = Some(snap(c));
        }
    });
    r
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SortBy {
    /// Total wait (default): where time is lost.
    #[default]
    Wait,
    Hold,
    Acquired,
    Contended,
    Name,
}

impl SortBy {
    pub fn parse(s: &str) -> Option<SortBy> {
        Some(match s {
            "wait" => SortBy::Wait,
            "hold" => SortBy::Hold,
            "acquired" => SortBy::Acquired,
            "contended" => SortBy::Contended,
            "name" => SortBy::Name,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct ReportOpts {
    /// Only classes whose name starts with this.
    pub prefix: Option<String>,
    pub sort: SortBy,
    /// At most this many class lines (0 = all).
    pub max: usize,
    /// Also classes never taken.
    pub all: bool,
}

fn time(out: &mut String, ticks: u64) {
    match ticks_to_ns(ticks) {
        Some(ns) if ns >= 10_000_000 => {
            let _ = write!(out, "{}ms", ns / 1_000_000);
        }
        Some(ns) => {
            let _ = write!(out, "{}.{}us", ns / 1000, (ns % 1000) / 100);
        }
        None => {
            let _ = write!(out, "{ticks}t");
        }
    }
}

/// The `locks` tool's answer: a head line, one line per class, then the checker's findings.
pub fn report(o: &ReportOpts) -> String {
    let mut v: Vec<ClassStats> = classes()
        .into_iter()
        .filter(|c| o.all || c.acquired > 0)
        .filter(|c| o.prefix.as_deref().is_none_or(|p| c.name.starts_with(p)))
        .collect();
    match o.sort {
        SortBy::Wait => v.sort_by(|a, b| b.wait_ticks.cmp(&a.wait_ticks).then(b.acquired.cmp(&a.acquired))),
        SortBy::Hold => v.sort_by_key(|c| core::cmp::Reverse(c.hold_ticks)),
        SortBy::Acquired => v.sort_by_key(|c| core::cmp::Reverse(c.acquired)),
        SortBy::Contended => v.sort_by(|a, b| b.contended.cmp(&a.contended).then(b.wait_ticks.cmp(&a.wait_ticks))),
        SortBy::Name => v.sort_by(|a, b| a.name.cmp(b.name)),
    }
    let total = v.len();
    if o.max > 0 {
        v.truncate(o.max);
    }
    let cnt = crate::lockdep::counts();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "locks classes={} shown={} checker={} violations={} unexpected={} injected={} unrecorded={} untracked={} tsc_hz={} irq_depth={}",
        total,
        v.len(),
        if crate::lockdep::enabled() { "on" } else { "off" },
        cnt.total,
        cnt.unexpected,
        cnt.injected,
        cnt.unrecorded,
        cnt.untracked,
        tsc_hz(),
        crate::cpu::irq_lock_depth(),
    );
    let _ = writeln!(out, "class level acquired contended wait_total wait_max hold_avg hold_max try_failed timeouts");
    for c in &v {
        let _ = write!(out, "{} {}:{} {} {} ", c.name, order::level_name(c.level), c.level, c.acquired, c.contended);
        time(&mut out, c.wait_ticks);
        out.push(' ');
        time(&mut out, c.wait_max_ticks);
        out.push(' ');
        time(&mut out, c.hold_ticks.checked_div(c.acquired).unwrap_or(0));
        out.push(' ');
        time(&mut out, c.hold_max_ticks);
        let _ = writeln!(out, " {} {}", c.try_failed, c.timeouts);
    }
    let list = crate::lockdep::list();
    if !list.is_empty() {
        let _ = writeln!(out, "violations (kind held -> taking: count, injected, first thread)");
        for x in list {
            let _ = writeln!(
                out,
                "{} {}:{} -> {}:{}: {} injected={} thread={}",
                x.kind.name(),
                x.held,
                x.held_level,
                x.taking,
                x.taking_level,
                x.count,
                x.injected,
                x.first_thread
            );
        }
    }
    out
}
