//! SC-7a (B4 "the desktop never freezes"): the Interactive class for user threads, granted by policy.
//!
//! A user thread belongs to a **group** (its process's tgid, `spawn_in_group`). A group can hold grants:
//! `Compositor` and `Input` (the compositor and the input path, set by whoever starts them) and `Focus` (the
//! program in front; exactly one group, `set_focus`, called by the compositor through its kernel interface:
//! fb0's `FBIO_BITSOS_COMPOSITOR`, tied to the screen's holder (gopfb fbdev/compositor.rs, INT-W4), also by the
//! MCP tool `sched_focus` and the ktests). Every Normal thread of a granted group runs in the
//! Interactive class (`attr::effective_class`): weight of nice -5, a full slice of wake-up credit, and it
//! preempts Normal and Background threads when it wakes while it is less than `run::INTERACTIVE_WAKE_NS` of
//! virtual time ahead of them. Interactive stays a fair class: it keeps its debt, so a thread in front that
//! runs and sleeps briefly in a loop still leaves the others their share by weight; nothing starves. A program never
//! raises itself: no syscall grants to its caller alone (the compositor ioctl needs the whole screen and B3's
//! yes); SCHED_FIFO/RR are accepted only inside a grant (`attr`).
//!
//! Grants live in a small list (`GRANTS`, grows on demand, R1). A grant ends when it is revoked, when the
//! focus moves on, or when the last thread of the group exits (`thread_exits`), so a reused pid never
//! inherits one. Liveness is a thread's `left` flag (set first thing in `thread_exits`, SeqCst), read under the
//! `GRANTS` lock by both sides: of two last threads exiting at once at least one sees the other gone and clears
//! the grants (store-then-load on both sides), and a grant is never added to a group whose last thread has
//! passed `thread_exits` (it either sees the flag, or `thread_exits` takes the lock after it and sees the grant).
//!
//! The metric "longest wait of an Interactive thread": the time from the wake-up that made an Interactive
//! thread runnable to its switch-in (`switched_in`, wake-to-run; a preempted thread waiting for its next turn is
//! fair sharing, not counted). Counters (RB-7b's `metrics`): `sched.interactive.waits`, `.wait_ns` (sum),
//! `.wait_max_ns` (the largest since boot; a running maximum is a counter that only grows), `.late` (waits of
//! `LATE_NS` or more); events `interactive.late` and `interactive.wait_max` (a new maximum above 1 ms); the
//! `sched` MCP tool also shows the largest wait of the last 10-20 s and per thread its own maximum.

use crate::{Class, Thread, table};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;

/// Why a group runs Interactive (bits; a group may hold several).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Grant {
    Compositor = 1,
    Input = 2,
    Focus = 4,
}

impl Grant {
    pub fn name(self) -> &'static str {
        match self {
            Grant::Compositor => "compositor",
            Grant::Input => "input",
            Grant::Focus => "focus",
        }
    }
    pub fn from_name(s: &str) -> Option<Grant> {
        [Grant::Compositor, Grant::Input, Grant::Focus].into_iter().find(|g| g.name() == s)
    }
}

/// The names of the grants in `bits`, comma-separated ("-" for none).
pub fn bits_names(bits: u8) -> alloc::string::String {
    let v: Vec<&str> = [Grant::Compositor, Grant::Input, Grant::Focus].into_iter().filter(|g| bits & *g as u8 != 0).map(|g| g.name()).collect();
    if v.is_empty() { alloc::string::String::from("-") } else { v.join(",") }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrantError {
    /// Group 0 (kernel threads) or a group without a live thread.
    NoGroup,
    /// No memory to grow the grant list.
    NoMemory,
}

/// (group, grant bits), never an entry with no bits. Lock order: `sched.attr` before `sched.grants`.
static GRANTS: SpinLock<Vec<(u64, u8)>> = SpinLock::named("sched.grants", Vec::new());
/// The group in front (0: none).
static FOCUS: AtomicU64 = AtomicU64::new(0);

klog::counter!(pub GRANT_CHANGES, "sched.interactive.grant_changes");
klog::counter!(pub WAITS, "sched.interactive.waits");
klog::counter!(pub WAIT_NS, "sched.interactive.wait_ns");
klog::counter!(pub WAIT_MAX_NS, "sched.interactive.wait_max_ns");
klog::counter!(pub LATE, "sched.interactive.late");

/// A wake-to-run wait of this length or more is "late" (the B4 budget: echo under 20 ms, V2-PLAN §1a).
pub const LATE_NS: u64 = 20_000_000;
/// New maxima below this are not worth an event.
const MAX_EVENT_FLOOR_NS: u64 = 1_000_000;
/// Width of one window of the recent maximum.
pub const WINDOW_NS: u64 = 10_000_000_000;

/// The grant bits of `group` (0 for group 0).
pub fn grants_of(group: u64) -> u8 {
    if group == 0 {
        return 0;
    }
    GRANTS.lock().iter().find(|e| e.0 == group).map_or(0, |e| e.1)
}

/// Whether `group` holds any grant.
pub fn granted(group: u64) -> bool {
    grants_of(group) != 0
}

/// Every granted group with its bits, by group.
pub fn grants() -> Vec<(u64, u8)> {
    let mut v = GRANTS.lock().clone();
    v.sort();
    v
}

/// The group in front (None: no focus set).
pub fn focus() -> Option<u64> {
    Some(FOCUS.load(Ordering::Acquire)).filter(|&g| g != 0)
}

/// Live threads of `group` (not exiting yet).
pub fn group_threads(group: u64) -> Vec<alloc::sync::Arc<Thread>> {
    if group == 0 {
        return Vec::new();
    }
    table::snapshot().into_iter().filter(|t| t.group() == group && t.in_group()).collect()
}

/// Whether `ts` (a snapshot of `group_threads`) still holds a live thread; called under `GRANTS`.
fn any_live(ts: &[alloc::sync::Arc<Thread>]) -> bool {
    ts.iter().any(|t| t.in_group())
}

/// Sets or clears `bit` of `group`'s entry (`GRANTS` held). Err only for no memory.
fn set_bit(v: &mut Vec<(u64, u8)>, group: u64, bit: u8, on: bool) -> Result<bool, GrantError> {
    match v.iter().position(|e| e.0 == group) {
        Some(i) => {
            let old = v[i].1;
            let new = if on { old | bit } else { old & !bit };
            if new == 0 {
                v.swap_remove(i);
            } else {
                v[i].1 = new;
            }
            Ok(old != new)
        }
        None if on => {
            v.try_reserve(1).map_err(|_| GrantError::NoMemory)?;
            v.push((group, bit));
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Recomputes the class of every live thread of `group`; returns how many there are.
fn refresh_group(group: u64) -> usize {
    let ts = group_threads(group);
    for t in &ts {
        crate::attr::refresh(t);
    }
    ts.len()
}

/// Grants (`on`) or revokes `grant` for `group`. Returns the number of the group's live threads.
pub fn set_grant(group: u64, grant: Grant, on: bool) -> Result<usize, GrantError> {
    if grant == Grant::Focus {
        return if on {
            set_focus(group).map(|_| group_threads(group).len())
        } else if focus() == Some(group) {
            set_focus(0).map(|_| 0)
        } else {
            Ok(group_threads(group).len())
        };
    }
    if group == 0 {
        return Err(GrantError::NoGroup);
    }
    let ts = if on { group_threads(group) } else { Vec::new() };
    let changed = {
        let mut g = GRANTS.lock();
        if on && !any_live(&ts) {
            return Err(GrantError::NoGroup);
        }
        set_bit(&mut g, group, grant as u8, on)?
    };
    let n = refresh_group(group);
    if changed {
        GRANT_CHANGES.inc();
        klog::event!(Info, "sched", "grant.set", group = group, grant = grant.name(), on = on, threads = n, grants = bits_names(grants_of(group)).as_str());
    }
    Ok(n)
}

/// Makes `group` the program in front (0: none). The group in front before loses its Focus grant. Returns the
/// group in front before (0: none).
pub fn set_focus(group: u64) -> Result<u64, GrantError> {
    let ts = group_threads(group);
    let old = {
        let mut g = GRANTS.lock();
        let old = FOCUS.load(Ordering::Acquire);
        if old == group {
            return Ok(old);
        }
        if group != 0 && !any_live(&ts) {
            return Err(GrantError::NoGroup);
        }
        if group != 0 {
            set_bit(&mut g, group, Grant::Focus as u8, true)?;
        }
        if old != 0 {
            let _ = set_bit(&mut g, old, Grant::Focus as u8, false);
        }
        FOCUS.store(group, Ordering::Release);
        old
    };
    let n = if group != 0 { refresh_group(group) } else { 0 };
    if old != 0 {
        refresh_group(old);
    }
    GRANT_CHANGES.inc();
    klog::event!(Info, "sched", "focus.set", group = group, before = old, threads = n);
    Ok(old)
}

/// `t` exits: the last live thread of a granted group takes the group's grants with it.
pub(crate) fn thread_exits(t: &Thread) {
    let g = t.group();
    if g == 0 {
        return;
    }
    // First, before any look at the grants (see the module comment for why this order closes both races).
    t.left.store(true, Ordering::SeqCst);
    if !granted(g) {
        return;
    }
    // Outside the lock (it allocates); a thread spawned into the group after it was taken was spawned by a live
    // member of the snapshot, so "no live member in it" stays true. An empty snapshot (no memory) clears.
    let others: Vec<alloc::sync::Arc<Thread>> = table::snapshot().into_iter().filter(|o| o.id != t.id && o.group() == g).collect();
    let bits = {
        let mut v = GRANTS.lock();
        if any_live(&others) {
            return;
        }
        let bits = v.iter().find(|e| e.0 == g).map_or(0, |e| e.1);
        v.retain(|e| e.0 != g);
        let _ = FOCUS.compare_exchange(g, 0, Ordering::AcqRel, Ordering::Acquire);
        bits
    };
    if bits != 0 {
        GRANT_CHANGES.inc();
        klog::event!(Info, "sched", "grant.ended", group = g, grants = bits_names(bits).as_str(); "the group's last thread exited");
    }
}

// ---------------------------------------------------------------------------------------------------------
// The wait metric

static MAX: AtomicU64 = AtomicU64::new(0);
/// Two windows of `WINDOW_NS` (by parity of the window number): the window number and its largest wait.
static WIN_EPOCH: [AtomicU64; 2] = [AtomicU64::new(u64::MAX), AtomicU64::new(u64::MAX)];
static WIN_MAX: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];

/// A wake-up made `t` runnable now (any context, also hard IRQ: a clock read and two atomics, only for an
/// Interactive thread).
pub(crate) fn woken(t: &Thread) {
    if t.class() == Class::Interactive {
        t.woken_at.store(time::now_ns().max(1), Ordering::Relaxed);
    }
}

/// `t` is switched in at `now` (the scheduler, interrupts off; no allocation, no lock).
pub(crate) fn switched_in(t: &Thread, now: u64) {
    let w = t.woken_at.swap(0, Ordering::Relaxed);
    if w == 0 || t.class() != Class::Interactive {
        return;
    }
    let d = now.saturating_sub(w);
    WAITS.inc();
    WAIT_NS.add(d);
    t.wait_max.fetch_max(d, Ordering::Relaxed);
    let old = MAX.fetch_max(d, Ordering::Relaxed);
    if d > old {
        // The increments of concurrent new maxima add up to the final maximum (each adds its step over the one
        // before), so the counter equals `MAX`.
        WAIT_MAX_NS.add(d - old);
    }
    note_window(now, d);
    if d >= LATE_NS {
        LATE.inc();
        // This runs inside `schedule` with interrupts off: at most one event per `LATE_EVENT_GAP_NS` (a stalled CPU
        // makes every thread queued there late at once); the counter keeps every one.
        if let Some(skipped) = late_event_due(now) {
            klog::event!(Info, "sched", "interactive.late", id = t.id, name = t.name.as_str(), group = t.group(), wait_us = d / 1000, cpu = t.last_cpu(), not_reported_before = skipped);
        }
    } else if d > old && d >= MAX_EVENT_FLOOR_NS {
        klog::event!(Info, "sched", "interactive.wait_max", id = t.id, name = t.name.as_str(), group = t.group(), wait_us = d / 1000);
    }
}

/// Minimum time between two `interactive.late` events.
const LATE_EVENT_GAP_NS: u64 = 100_000_000;
static LATE_EVENT_AT: AtomicU64 = AtomicU64::new(0);
static LATE_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Whether a late wait at `now` may be an event; Some(late waits not reported since the last event).
fn late_event_due(now: u64) -> Option<u64> {
    let last = LATE_EVENT_AT.load(Ordering::Relaxed);
    if (last != 0 && now.saturating_sub(last) < LATE_EVENT_GAP_NS) || LATE_EVENT_AT.compare_exchange(last, now.max(1), Ordering::AcqRel, Ordering::Relaxed).is_err() {
        LATE_SKIPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(LATE_SKIPPED.swap(0, Ordering::Relaxed))
}

fn note_window(now: u64, d: u64) {
    let e = now / WINDOW_NS;
    let i = (e & 1) as usize;
    if WIN_EPOCH[i].load(Ordering::Acquire) != e && WIN_EPOCH[i].swap(e, Ordering::AcqRel) != e {
        // A new window: a wait recorded between the swap and this store may be lost (display only).
        WIN_MAX[i].store(0, Ordering::Relaxed);
    }
    WIN_MAX[i].fetch_max(d, Ordering::Relaxed);
}

/// The Interactive wait statistics.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WaitStats {
    pub waits: u64,
    pub total_ns: u64,
    /// Largest since boot.
    pub max_ns: u64,
    /// Largest in this and the previous window (the last 10-20 s).
    pub recent_max_ns: u64,
    pub late: u64,
}

pub fn wait_stats_now() -> WaitStats {
    wait_stats(time::now_ns())
}

pub fn wait_stats(now: u64) -> WaitStats {
    let e = now / WINDOW_NS;
    let recent = (0..2)
        .filter(|&i| {
            let we = WIN_EPOCH[i].load(Ordering::Acquire);
            we == e || we.wrapping_add(1) == e
        })
        .map(|i| WIN_MAX[i].load(Ordering::Relaxed))
        .max()
        .unwrap_or(0);
    WaitStats { waits: WAITS.get(), total_ns: WAIT_NS.get(), max_ns: MAX.load(Ordering::Relaxed), recent_max_ns: recent, late: LATE.get() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attr::{Policy, effective_class, rt_change_allowed};

    #[test]
    fn classes_follow_spawn_class_policy_and_grant() {
        use Class::*;
        for p in [Policy::Other, Policy::Ext, Policy::Fifo, Policy::Rr] {
            assert_eq!(effective_class(Normal, p, true), Interactive, "{p:?}");
            assert_eq!(effective_class(Normal, p, false), Normal, "{p:?}");
        }
        assert_eq!(effective_class(Normal, Policy::Idle, true), Background, "SCHED_IDLE is Background, also in front");
        assert_eq!(effective_class(Normal, Policy::Idle, false), Background);
        assert_eq!(effective_class(Normal, Policy::Batch, true), Normal, "batch is never Interactive");
        assert_eq!(effective_class(KernelRt, Policy::Idle, true), KernelRt);
        assert_eq!(effective_class(Background, Policy::Other, true), Background);
        assert_eq!(effective_class(Interactive, Policy::Other, false), Interactive, "a kernel Interactive thread stays");
    }

    #[test]
    fn rt_needs_the_grant_except_to_stay_or_go_down() {
        assert!(rt_change_allowed(Policy::Other, 0, Policy::Fifo, 10, true));
        assert!(!rt_change_allowed(Policy::Other, 0, Policy::Fifo, 10, false));
        assert!(!rt_change_allowed(Policy::Fifo, 10, Policy::Rr, 10, false), "changing the RT policy");
        assert!(!rt_change_allowed(Policy::Fifo, 10, Policy::Fifo, 11, false), "raising");
        assert!(rt_change_allowed(Policy::Fifo, 10, Policy::Fifo, 10, false));
        assert!(rt_change_allowed(Policy::Rr, 10, Policy::Rr, 1, false), "lowering");
    }

    #[test]
    fn late_events_are_spaced() {
        let t0 = 5 * LATE_EVENT_GAP_NS;
        assert_eq!(late_event_due(t0), Some(0));
        assert_eq!(late_event_due(t0 + 1), None);
        assert_eq!(late_event_due(t0 + LATE_EVENT_GAP_NS / 2), None);
        assert_eq!(late_event_due(t0 + LATE_EVENT_GAP_NS), Some(2), "the two skipped ones are reported");
        assert_eq!(late_event_due(t0 + LATE_EVENT_GAP_NS + 1), None);
    }

    #[test]
    fn grant_bits() {
        let mut v = Vec::new();
        assert_eq!(set_bit(&mut v, 7, Grant::Focus as u8, true), Ok(true));
        assert_eq!(set_bit(&mut v, 7, Grant::Input as u8, true), Ok(true));
        assert_eq!(set_bit(&mut v, 7, Grant::Input as u8, true), Ok(false));
        assert_eq!(v, alloc::vec![(7, 6)]);
        assert_eq!(bits_names(6), "input,focus");
        assert_eq!(set_bit(&mut v, 7, Grant::Focus as u8, false), Ok(true));
        assert_eq!(set_bit(&mut v, 7, Grant::Input as u8, false), Ok(true));
        assert!(v.is_empty(), "no entry without bits");
        assert_eq!(set_bit(&mut v, 8, Grant::Input as u8, false), Ok(false));
        assert_eq!(bits_names(0), "-");
        assert_eq!(Grant::from_name("compositor"), Some(Grant::Compositor));
        assert_eq!(Grant::from_name("x"), None);
    }
}
