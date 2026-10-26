//! Where a thread runs (SC-3; pure, host-tested): wake placement and the targets of balancing, on the topology
//! CP-4 reads from CPUID (R4: topology is data). Allocation-free, because interrupt-time wake-ups use it too.
//!
//! Wake placement (Linux `select_idle_sibling` in small): the CPU the thread ran on last if it is idle (warm
//! caches); else an idle CPU, preferring one in the same last-level-cache cluster (CCX / L3), then one whose
//! whole physical core is idle (no busy SMT sibling); else the least busy CPU. On hybrid CPUs (P and E cores)
//! KernelRt and Interactive threads prefer P cores and Background threads E cores; Normal ones take either.

/// What placement needs to know about one online CPU.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CpuView {
    /// Runs its idle thread and has nothing queued.
    pub idle: bool,
    /// Threads running or waiting there.
    pub load: usize,
    /// The physical core (package, core): SMT siblings share it.
    pub core: (u32, u32),
    /// The last-level-cache domain (package, cluster).
    pub cluster: (u32, u32),
    /// `hal::smp::CORE_TYPE_*`.
    pub core_type: u8,
    /// CP-6 capacity hint, `hal::smp::CAPACITY_SCALE` (1024) for the fastest core; below it a slower or
    /// not-favoured core.
    pub capacity: u16,
}

/// Which core type a thread likes on a hybrid CPU.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pref {
    Any,
    Performance,
    Efficiency,
}

fn type_penalty(pref: Pref, core_type: u8, hybrid: bool) -> usize {
    if !hybrid {
        return 0;
    }
    match pref {
        Pref::Any => 0,
        Pref::Performance => usize::from(core_type != hal::smp::CORE_TYPE_PERFORMANCE),
        Pref::Efficiency => usize::from(core_type != hal::smp::CORE_TYPE_EFFICIENCY),
    }
}

/// Whether every online CPU of `v`'s physical core is idle.
fn core_idle(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, v: &CpuView) -> bool {
    (0..n).filter_map(view).all(|o| o.core != v.core || o.idle)
}

/// Picks a CPU among `0..n` (`view` gives None for CPUs that are not online). `prev`: where the thread ran last.
/// `None` only when no CPU is online.
pub fn select(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, prev: Option<usize>, pref: Pref) -> Option<usize> {
    let hybrid = (0..n).filter_map(view).any(|v| v.core_type != hal::smp::CORE_TYPE_UNIFORM);
    let pv = prev.and_then(view);
    let near = |v: &CpuView| pv.is_none_or(|p| p.cluster == v.cluster);
    // 1. An idle CPU: the previous one if it is idle and of the liked type, else the best idle one.
    let mut best: Option<(usize, usize)> = None;
    for c in 0..n {
        let Some(v) = view(c) else { continue };
        if !v.idle {
            continue;
        }
        // Lowest weight: Interactive and KernelRt threads take the favoured (full-capacity) cores first.
        let score = type_penalty(pref, v.core_type, hybrid) * 16
            + usize::from(!near(&v)) * 8
            + usize::from(Some(c) != prev) * 4
            + usize::from(!core_idle(n, view, &v)) * 2
            + usize::from(v.capacity < hal::smp::CAPACITY_SCALE && pref == Pref::Performance);
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((c, score));
        }
    }
    if let Some((c, _)) = best {
        return Some(c);
    }
    // 2. The least busy CPU (the previous one on a tie, then the same cluster).
    let mut best: Option<(usize, usize)> = None;
    for c in 0..n {
        let Some(v) = view(c) else { continue };
        // The load counts relative to the core's capacity (CP-6).
        let score = v.load * 16 * usize::from(hal::smp::CAPACITY_SCALE) / usize::from(v.capacity.max(1)) + type_penalty(pref, v.core_type, hybrid) * 8 + usize::from(!near(&v)) * 2 + usize::from(Some(c) != prev);
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((c, score));
        }
    }
    best.map(|(c, _)| c)
}

/// The CPU an overloaded CPU `from` should give a thread to: an idle one (same cluster first), else one that
/// runs at least two threads fewer. `None`: balanced enough.
pub fn push_target(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, from: usize) -> Option<usize> {
    let me = view(from)?;
    let mut best: Option<(usize, usize)> = None;
    for c in 0..n {
        if c == from {
            continue;
        }
        let Some(v) = view(c) else { continue };
        let score = if v.idle {
            usize::from(v.cluster != me.cluster)
        } else if v.load + 2 <= me.load {
            16 + v.load * 4 + usize::from(v.cluster != me.cluster)
        } else {
            continue;
        };
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((c, score));
        }
    }
    best.map(|(c, _)| c)
}

/// The CPU an idle CPU `me` should take work from: the one with the most threads waiting (at least one), the
/// same cluster preferred on a tie. `waiting(c)` = threads queued on `c` that are not running.
pub fn steal_victim(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, waiting: &dyn Fn(usize) -> usize, me: usize) -> Option<usize> {
    let mine = view(me)?;
    let mut best: Option<(usize, usize)> = None;
    for c in 0..n {
        if c == me || view(c).is_none() {
            continue;
        }
        let w = waiting(c);
        if w == 0 {
            continue;
        }
        let same = view(c).is_some_and(|v| v.cluster == mine.cluster);
        let score = w * 2 + usize::from(same);
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((c, score));
        }
    }
    best.map(|(c, _)| c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use hal::smp::{CORE_TYPE_EFFICIENCY as E, CORE_TYPE_PERFORMANCE as P, CORE_TYPE_UNIFORM as U};

    /// Mini-PC shape: 8 cores x 2 threads, one CCX; CPU numbers 0..16 with siblings (2k, 2k+1).
    fn smt16() -> Vec<CpuView> {
        (0..16).map(|c| CpuView { idle: false, load: 1, core: (0, c / 2), cluster: (0, 0), core_type: U, capacity: 1024 }).collect()
    }

    fn sel(v: &[CpuView], prev: Option<usize>, pref: Pref) -> Option<usize> {
        let f = |c: usize| v.get(c).copied();
        select(v.len(), &f, prev, pref)
    }

    #[test]
    fn previous_cpu_when_idle() {
        let mut v = smt16();
        v[5].idle = true;
        v[5].load = 0;
        v[9].idle = true;
        v[9].load = 0;
        v[8].idle = true;
        v[8].load = 0;
        assert_eq!(sel(&v, Some(5), Pref::Any), Some(5), "warm caches first, even with a busy sibling");
    }

    #[test]
    fn a_whole_idle_core_beats_an_idle_smt_sibling() {
        let mut v = smt16();
        for c in [3, 10, 11] {
            v[c].idle = true;
            v[c].load = 0;
        }
        // prev=0 is busy: CPU 3's sibling 2 is busy, core 5 (10, 11) is idle as a whole.
        assert_eq!(sel(&v, Some(0), Pref::Any), Some(10));
    }

    #[test]
    fn same_cluster_first() {
        // Two CCX of 4 cores (no SMT): 0-3 cluster 0, 4-7 cluster 1.
        let mut v: Vec<CpuView> = (0..8).map(|c| CpuView { idle: false, load: 1, core: (0, c), cluster: (0, c / 4), core_type: U, capacity: 1024 }).collect();
        v[1].idle = true;
        v[1].load = 0;
        v[6].idle = true;
        v[6].load = 0;
        assert_eq!(sel(&v, Some(5), Pref::Any), Some(6));
        assert_eq!(sel(&v, Some(2), Pref::Any), Some(1));
    }

    #[test]
    fn least_loaded_when_nothing_is_idle() {
        let mut v = smt16();
        for (c, x) in v.iter_mut().enumerate() {
            x.load = 3 + c % 3;
        }
        v[7].load = 2;
        assert_eq!(sel(&v, Some(1), Pref::Any), Some(7));
        v[1].load = 2;
        assert_eq!(sel(&v, Some(1), Pref::Any), Some(1), "a tie keeps the previous CPU");
    }

    #[test]
    fn hybrid_interactive_prefers_p_background_prefers_e() {
        // Arrow Lake-like: CPUs 0-7 P cores, 8-19 E cores (no SMT on either).
        let mut v: Vec<CpuView> = (0..20)
            .map(|c| CpuView { idle: true, load: 0, core: (0, c), cluster: (0, if c < 8 { 0 } else { 1 + (c - 8) / 4 }), core_type: if c < 8 { P } else { E }, capacity: if c < 8 { 1024 } else { 756 } })
            .collect();
        assert!(sel(&v, Some(12), Pref::Performance).is_some_and(|c| c < 8), "an Interactive thread leaves its E core");
        assert_eq!(sel(&v, Some(12), Pref::Efficiency), Some(12));
        assert!(sel(&v, Some(3), Pref::Efficiency).is_some_and(|c| c >= 8));
        assert_eq!(sel(&v, Some(3), Pref::Any), Some(3));
        // All P cores busy: an Interactive thread takes an idle E core rather than waiting.
        for x in v.iter_mut().take(8) {
            x.idle = false;
            x.load = 1;
        }
        assert!(sel(&v, Some(3), Pref::Performance).is_some_and(|c| c >= 8));
    }

    #[test]
    fn capacity_hint_orders_cores() {
        // Arrow Lake-like favoured cores: CPU 2 is the fastest P core (1024), the others 1012, CPU 3 an E core (756).
        let caps = [1012u16, 1012, 1024, 756];
        let mut v: Vec<CpuView> = (0..4)
            .map(|c| CpuView { idle: true, load: 0, core: (0, c as u32), cluster: (0, 0), core_type: U, capacity: caps[c] })
            .collect();
        assert_eq!(sel(&v, None, Pref::Performance), Some(2), "an Interactive thread takes the favoured core");
        assert_eq!(sel(&v, Some(0), Pref::Any), Some(0), "a Normal thread stays where it was");
        // Nothing idle, equal loads: the load counts relative to capacity, so the slow core looks busier.
        for x in v.iter_mut() {
            x.idle = false;
            x.load = 2;
        }
        assert_ne!(sel(&v, Some(3), Pref::Any), Some(3));
        assert_eq!(sel(&v, Some(2), Pref::Any), Some(2));
    }

    #[test]
    fn offline_cpus_are_never_chosen() {
        let v = smt16();
        let f = |c: usize| if c == 4 { Some(CpuView { idle: true, load: 0, ..v[4] }) } else { None };
        assert_eq!(select(16, &f, Some(0), Pref::Any), Some(4));
        let none = |_c: usize| None;
        assert_eq!(select(16, &none, Some(0), Pref::Any), None);
    }

    #[test]
    fn push_and_steal_targets() {
        let mut v = smt16();
        v[0].load = 5;
        // Everybody else runs one thread: the first CPU at least two below takes one.
        let f = |c: usize| v.get(c).copied();
        assert_eq!(push_target(16, &f, 0), Some(1));
        for x in v.iter_mut().skip(1) {
            x.load = 4;
        }
        let f = |c: usize| v.get(c).copied();
        assert_eq!(push_target(16, &f, 0), None, "4 vs 5 is balanced");
        v[9].load = 2;
        let f = |c: usize| v.get(c).copied();
        assert_eq!(push_target(16, &f, 0), Some(9));
        v[12].idle = true;
        v[12].load = 0;
        let f = |c: usize| v.get(c).copied();
        assert_eq!(push_target(16, &f, 0), Some(12), "an idle CPU first");
        let waiting = |c: usize| if c == 3 { 2 } else if c == 6 { 4 } else { 0 };
        assert_eq!(steal_victim(16, &f, &waiting, 12), Some(6));
        let nothing = |_c: usize| 0;
        assert_eq!(steal_victim(16, &f, &nothing, 12), None);
    }
}
