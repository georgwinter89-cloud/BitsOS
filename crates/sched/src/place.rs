//! Where a thread runs (SC-3; pure, host-tested): wake placement and the targets of balancing, on the topology
//! CP-4 reads from CPUID (R4: topology is data). Allocation-free, because interrupt-time wake-ups use it too.
//!
//! Wake placement (Linux `select_idle_sibling` in small): the CPU the thread ran on last if it is idle (warm
//! caches); else an idle CPU, preferring one in the same last-level-cache cluster (CCX / L3), then one whose
//! whole physical core is idle (no busy SMT sibling); else the least busy CPU.
//!
//! Hybrid CPUs (P and E cores; CP-265K, on the numbers of a real 265K, Linux's asym-capacity in small, reference
//! linux-7.3.1/kernel/sched/fair.c:9181 `select_idle_sibling`, :9153 `asym_fits_cpu`): Background threads take E cores
//! first, every other thread a free P core first, because the scheduler keeps no per-thread busy signal to tell a
//! light thread from a busy one, and E cores once the P cores are busy. Inside that order the previous CPU if it is
//! idle (warm caches beat the favoured core's 1 %), then a whole idle core, the higher capacity (favoured cores first
//! when the numbers say so), the previous L2 (E cores share one in fours), the L2 with the fewest busy CPUs. A uniform
//! CPU never takes this path and never asks for an L2: its answers are the same as before, a test holds them against
//! the old code.

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
    /// GP-CPU2: a light Normal thread on a hybrid CPU: its previous core if idle (any kind), else E cores first, so
    /// the P cores stay free for busy and foreground threads (`hybrid::pref_for`). On a uniform CPU it is `Any`.
    Light,
}

fn type_penalty(pref: Pref, core_type: u8, hybrid: bool) -> usize {
    if !hybrid {
        return 0;
    }
    match pref {
        Pref::Any => 0,
        Pref::Performance => usize::from(core_type != hal::smp::CORE_TYPE_PERFORMANCE),
        Pref::Efficiency | Pref::Light => usize::from(core_type != hal::smp::CORE_TYPE_EFFICIENCY),
    }
}

/// Whether every online CPU of `v`'s physical core is idle.
fn core_idle(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, v: &CpuView) -> bool {
    (0..n).filter_map(view).all(|o| o.core != v.core || o.idle)
}

/// The L2 domain of a CPU, asked only on hybrid CPUs.
fn hal_l2(c: usize) -> Option<u32> {
    hal::smp::topology(c).map(|t| t.l2_id)
}

/// An idle CPU for the thread on a hybrid machine: the lowest key wins, field by field (see the module text). This runs
/// at interrupt time and each sibling or neighbour question costs a walk over all CPUs, so they are asked only where
/// they can change the answer: a CPU that cannot beat the best so far even with no busy neighbour and a whole idle core
/// asks nothing, CPUs of one L2 in a row count its busy CPUs once, and the sibling question comes last. Machines with
/// mixed-up CPU numbers count more often; the answer stays the same, a test holds it to the plain walk.
fn hybrid_idle(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, l2: &dyn Fn(usize) -> Option<u32>, prev: Option<usize>, pref: Pref) -> Option<usize> {
    let liked = if matches!(pref, Pref::Efficiency | Pref::Light) { hal::smp::CORE_TYPE_EFFICIENCY } else { hal::smp::CORE_TYPE_PERFORMANCE };
    if let Some(p) = prev
        && view(p).is_some_and(|v| v.idle && (v.core_type == liked || pref == Pref::Light))
    {
        return Some(p);
    }
    let pv = prev.and_then(view);
    let prev_l2 = prev.and_then(l2);
    // (CPU, liked type and previous CPU, a busy sibling, capacity lost, other L2, busy CPUs in its L2, other cluster).
    let mut best: Option<(usize, (bool, bool), bool, (u16, bool, usize, bool))> = None;
    let mut counted: Option<(Option<u32>, usize)> = None;
    for c in 0..n {
        let Some(v) = view(c).filter(|v| v.idle) else { continue };
        let first = (v.core_type != liked, Some(c) != prev);
        if best.is_some_and(|(_, f, _, _)| first > f) {
            continue;
        }
        let mine = l2(c);
        let tail = |busy: usize| (hal::smp::CAPACITY_SCALE.saturating_sub(v.capacity), prev_l2.is_none() || prev_l2 != mine, busy, pv.is_some_and(|p| p.cluster != v.cluster));
        let beaten = |t| best.is_some_and(|(_, f, busy_core, b)| f == first && !busy_core && t >= b);
        if beaten(tail(0)) {
            continue;
        }
        let busy = match counted {
            Some((l, k)) if l == mine => k,
            _ => (0..n).filter(|&o| l2(o) == mine && view(o).is_some_and(|x| !x.idle)).count(),
        };
        counted = Some((mine, busy));
        let t = tail(busy);
        if beaten(t) {
            continue;
        }
        let busy_core = !core_idle(n, view, &v);
        if best.is_none_or(|(_, f, bc, b)| (first, busy_core, t) < (f, bc, b)) {
            best = Some((c, first, busy_core, t));
        }
    }
    best.map(|(c, ..)| c)
}

/// Picks a CPU among `0..n` (`view` gives None for CPUs that are not online). `prev`: where the thread ran last.
/// `None` only when no CPU is online.
pub fn select(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, prev: Option<usize>, pref: Pref) -> Option<usize> {
    select_by(n, view, &hal_l2, prev, pref)
}

/// `select` with the L2 domains given (`l2`; the tests' machines are not `hal`'s).
fn select_by(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, l2: &dyn Fn(usize) -> Option<u32>, prev: Option<usize>, pref: Pref) -> Option<usize> {
    let hybrid = (0..n).filter_map(view).any(|v| v.core_type != hal::smp::CORE_TYPE_UNIFORM);
    if hybrid && let Some(c) = hybrid_idle(n, view, l2, prev, pref) {
        return Some(c);
    }
    let pv = prev.and_then(view);
    let near = |c: usize, v: &CpuView| {
        if hybrid { prev.is_none_or(|p| l2(p) == l2(c)) } else { pv.is_none_or(|p| p.cluster == v.cluster) }
    };
    // 1. An idle CPU: the previous one if it is idle and of the liked type, else the best idle one.
    let mut best: Option<(usize, usize)> = None;
    for c in 0..n {
        let Some(v) = view(c) else { continue };
        if !v.idle {
            continue;
        }
        // Lowest weight: Interactive and KernelRt threads take the favoured (full-capacity) cores first.
        let score = type_penalty(pref, v.core_type, hybrid) * 16
            + usize::from(!near(c, &v)) * 8
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
        let score = v.load * 16 * usize::from(hal::smp::CAPACITY_SCALE) / usize::from(v.capacity.max(1)) + type_penalty(pref, v.core_type, hybrid) * 8 + usize::from(!near(c, &v)) * 2 + usize::from(Some(c) != prev);
        if best.is_none_or(|(_, s)| score < s) {
            best = Some((c, score));
        }
    }
    best.map(|(c, _)| c)
}

/// GP-CPU2: the idle core a misfit thread on `from` should move up to (Linux's misfit pull): an idle CPU whose
/// capacity is clearly greater (`hybrid::capacity_greater`), the highest capacity first, then the lowest number.
/// `None` on a uniform CPU (every capacity is the same) or when no faster core is idle.
pub fn misfit_target(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, from: usize) -> Option<usize> {
    let me = view(from)?;
    (0..n)
        .filter(|&c| c != from)
        .filter_map(|c| view(c).filter(|v| v.idle && crate::hybrid::capacity_greater(v.capacity, me.capacity)).map(|v| (c, v.capacity)))
        .max_by_key(|&(c, cap)| (cap, core::cmp::Reverse(c)))
        .map(|(c, _)| c)
}

/// The CPU an overloaded CPU `from` should give a thread to: an idle one (same cluster first), else one that
/// runs at least two threads fewer. `None`: balanced enough.
pub fn push_target(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, from: usize) -> Option<usize> {
    push_target_by(n, view, &hal_l2, from)
}

/// `push_target` with the L2 domains given. On a hybrid CPU the higher capacity wins among the idle CPUs, then the
/// same L2: an E core's waiting thread goes to a free P core first.
fn push_target_by(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, l2: &dyn Fn(usize) -> Option<u32>, from: usize) -> Option<usize> {
    let me = view(from)?;
    let hybrid = me.core_type != hal::smp::CORE_TYPE_UNIFORM;
    // Above any idle CPU's score, whatever its capacity.
    let busy_base = if hybrid { usize::from(hal::smp::CAPACITY_SCALE) * 8 } else { 16 };
    let mut best: Option<(usize, usize)> = None;
    for c in 0..n {
        if c == from {
            continue;
        }
        let Some(v) = view(c) else { continue };
        let score = if v.idle {
            let hybrid_rank = if hybrid { usize::from(hal::smp::CAPACITY_SCALE.saturating_sub(v.capacity)) * 4 + usize::from(l2(c) != l2(from)) * 2 } else { 0 };
            hybrid_rank + usize::from(v.cluster != me.cluster)
        } else if v.load + 2 <= me.load {
            busy_base + v.load * 4 + usize::from(v.cluster != me.cluster)
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
/// same cluster preferred on a tie (on a hybrid CPU first a slower core, then the same L2). `waiting(c)` = threads queued on `c` that are
/// not running.
pub fn steal_victim(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, waiting: &dyn Fn(usize) -> usize, me: usize) -> Option<usize> {
    steal_victim_by(n, view, &hal_l2, waiting, me)
}

/// `steal_victim` with the L2 domains given.
fn steal_victim_by(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, l2: &dyn Fn(usize) -> Option<u32>, waiting: &dyn Fn(usize) -> usize, me: usize) -> Option<usize> {
    let mine = view(me)?;
    let hybrid = mine.core_type != hal::smp::CORE_TYPE_UNIFORM;
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
        // GP-CPU2: on a hybrid CPU a faster idle core pulls from slower ones first (Linux's asym-capacity balance
        // moves work up); one more waiting thread still outweighs it.
        let slower = hybrid && view(c).is_some_and(|v| crate::hybrid::capacity_greater(mine.capacity, v.capacity));
        let score = w * 8 + usize::from(slower) * 4 + usize::from(same) * 2 + usize::from(hybrid && l2(c) == l2(me));
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((c, score));
        }
    }
    best.map(|(c, _)| c)
}

#[cfg(test)]
#[path = "place_gp_tests.rs"]
mod gp_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use hal::smp::{CORE_TYPE_EFFICIENCY as E, CORE_TYPE_PERFORMANCE as P, CORE_TYPE_UNIFORM as U};

    /// The real 265K as Linux shows it (read from the machine on 5.11.2026): capacity, core and L2 of all 20 CPUs,
    /// nothing idle-related yet: every CPU idle.
    struct Real265k {
        v: Vec<CpuView>,
        l2: Vec<u32>,
    }

    fn real_265k() -> Real265k {
        let mut r = Real265k { v: Vec::new(), l2: Vec::new() };
        for (i, l) in include_str!("../../hal/src/cpu/real_265k/topology.txt").lines().enumerate() {
            let field = |name: &str| l.split_whitespace().find_map(|f| f.strip_prefix(name)?.strip_prefix('=')).unwrap();
            r.l2.push(field("l2").split('-').next().unwrap().parse().unwrap());
            r.v.push(CpuView {
                idle: true,
                load: 0,
                core: (0, field("core").parse().unwrap()),
                cluster: (0, 0),
                core_type: if i < 8 { P } else { E },
                capacity: field("cap").parse().unwrap(),
            });
        }
        r
    }

    impl Real265k {
        fn busy(&mut self, cpus: impl IntoIterator<Item = usize>) {
            for c in cpus {
                self.v[c].idle = false;
                self.v[c].load = self.v[c].load.max(1);
            }
        }
        fn sel(&self, prev: Option<usize>, pref: Pref) -> Option<usize> {
            let f = |c: usize| self.v.get(c).copied();
            let l2 = |c: usize| self.l2.get(c).copied();
            select_by(self.v.len(), &f, &l2, prev, pref)
        }
    }

    #[test]
    fn real_265k_free_p_cores_first_favoured_first() {
        let mut m = real_265k();
        assert_eq!((m.v[3].capacity, m.v[5].capacity, m.v[0].capacity, m.v[8].capacity), (1024, 1024, 1012, 802), "the real numbers");
        for pref in [Pref::Any, Pref::Performance] {
            assert_eq!(m.sel(None, pref), Some(3), "a new thread: the first favoured core");
            assert_eq!(m.sel(Some(12), pref), Some(3), "a waking thread leaves its E core for a free P core");
            assert_eq!(m.sel(Some(0), pref), Some(0), "warm caches on a free P core beat the favoured core's 1 %");
        }
        // The favoured cores go first, then the others, then E cores: the 21st thread would queue.
        let mut order = Vec::new();
        for _ in 0..20 {
            let c = m.sel(None, Pref::Any).unwrap();
            order.push(c);
            m.busy([c]);
        }
        assert_eq!(&order[..8], [3, 5, 0, 1, 2, 4, 6, 7], "favoured, then the P cores");
        assert!(order[8..].iter().all(|&c| c >= 8));
        // E clusters fill one thread each before any shares an L2.
        let first_l2: Vec<u32> = order[8..11].iter().map(|&c| m.l2[c]).collect();
        assert_eq!(first_l2, [8, 12, 16], "{order:?}");
    }

    #[test]
    fn real_265k_e_cores_when_the_p_cores_are_busy() {
        let mut m = real_265k();
        m.busy(0..8);
        assert_eq!(m.sel(Some(13), Pref::Any), Some(13), "the previous E core if it is idle");
        assert_eq!(m.sel(Some(3), Pref::Performance), Some(8), "an idle E core rather than waiting");
        m.busy([13]);
        assert_eq!(m.sel(Some(13), Pref::Any), Some(12), "the previous E core is busy: its L2 neighbour");
        m.busy([12, 14, 15]);
        assert_eq!(m.sel(Some(13), Pref::Any), Some(8), "that L2 is full: another cluster");
        m.busy([9, 10, 11]);
        assert_eq!(m.sel(Some(13), Pref::Any), Some(16), "the L2 with the fewest busy CPUs: 8 is idle, but its L2 is not");
    }

    #[test]
    fn real_265k_background_takes_e_cores_near_its_l2() {
        let mut m = real_265k();
        assert_eq!(m.sel(Some(3), Pref::Efficiency), Some(8));
        assert_eq!(m.sel(Some(17), Pref::Efficiency), Some(17));
        m.busy([17]);
        assert_eq!(m.sel(Some(17), Pref::Efficiency), Some(16), "same L2");
        m.busy(8..20);
        assert_eq!(m.sel(Some(17), Pref::Efficiency), Some(3), "all E cores busy: a P core rather than waiting");
    }

    #[test]
    fn real_265k_nothing_idle_counts_the_load_by_capacity() {
        let mut m = real_265k();
        m.busy(0..20);
        // One thread everywhere: a P core is less busy than an E core.
        assert!(m.sel(Some(12), Pref::Any).is_some_and(|c| c < 8));
        for c in 0..8 {
            m.v[c].load = 2;
        }
        assert!(m.sel(Some(0), Pref::Any).is_some_and(|c| c >= 8), "two on each P core: an E core with one");
        assert_eq!(m.sel(Some(13), Pref::Any), Some(13), "and the previous one on a tie");
        assert_eq!(m.sel(Some(13), Pref::Efficiency), Some(13));
    }

    #[test]
    fn real_265k_push_and_steal() {
        let mut m = real_265k();
        m.busy(8..12);
        m.v[9].load = 3;
        let l2 = |c: usize| m.l2.get(c).copied();
        let push = |v: &[CpuView], from| {
            let f = |c: usize| v.get(c).copied();
            push_target_by(20, &f, &l2, from)
        };
        assert_eq!(push(&m.v, 9), Some(3), "a waiting thread on an E core goes to a free favoured P core");
        let mut v = m.v.clone();
        for x in v.iter_mut().take(8) {
            x.idle = false;
            x.load = 1;
        }
        assert_eq!(push(&v, 9), Some(12), "no P core free: an idle E core (the own L2 is busy)");
        for x in v.iter_mut().skip(8) {
            x.idle = false;
            x.load = 3;
        }
        v[10].load = 1;
        assert_eq!(push(&v, 9), Some(0), "nothing idle: the first CPU two below");
        let f = |c: usize| v.get(c).copied();
        let waiting = |c: usize| usize::from(c == 9 || c == 13);
        // CPU 8 is in CPU 9's L2.
        assert_eq!(steal_victim_by(20, &f, &l2, &waiting, 8), Some(9), "a tie: the same L2");
        assert_eq!(steal_victim_by(20, &f, &l2, &waiting, 12), Some(13));
    }

    // The plain walk: every field of every idle CPU, the answers `hybrid_idle` must keep.
    fn plain_hybrid_idle(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, l2: &dyn Fn(usize) -> Option<u32>, prev: Option<usize>, pref: Pref) -> Option<usize> {
        let liked = if pref == Pref::Efficiency { E } else { P };
        if let Some(p) = prev
            && view(p).is_some_and(|v| v.idle && v.core_type == liked)
        {
            return Some(p);
        }
        let pv = prev.and_then(view);
        let prev_l2 = prev.and_then(l2);
        let mut best: Option<(usize, ((bool, bool), (bool, u16, bool, usize, bool)))> = None;
        for c in 0..n {
            let Some(v) = view(c).filter(|v| v.idle) else { continue };
            let mine = l2(c);
            let key = (
                (v.core_type != liked, Some(c) != prev),
                (
                    !core_idle(n, view, &v),
                    1024u16.saturating_sub(v.capacity),
                    prev_l2.is_none() || prev_l2 != mine,
                    (0..n).filter(|&o| l2(o) == mine && view(o).is_some_and(|x| !x.idle)).count(),
                    pv.is_some_and(|p| p.cluster != v.cluster),
                ),
            );
            if best.is_none_or(|(_, k)| key < k) {
                best = Some((c, key));
            }
        }
        best.map(|(c, _)| c)
    }

    /// The asking-less walk answers like the plain one: on the real 265K, on a hybrid CPU with SMT P cores, and on
    /// mixed-up numbers where an L2's CPUs are not neighbours and some have no L2 known.
    #[test]
    fn hybrid_idle_answers_like_the_plain_walk() {
        let real = real_265k();
        let smt: Vec<CpuView> = (0..24u32)
            .map(|c| {
                let p = c < 16;
                CpuView { idle: true, load: 0, core: (0, if p { c / 2 } else { c }), cluster: (0, 0), core_type: if p { P } else { E }, capacity: if p { 1024 - (c / 2 % 3) as u16 * 12 } else { 700 } }
            })
            .collect();
        let mixed: Vec<CpuView> = (0..24u32)
            .map(|c| CpuView { idle: true, load: 0, core: (0, c % 11), cluster: (c % 2, c % 3), core_type: if c % 4 == 0 { E } else { P }, capacity: if c % 4 == 0 { 802 } else { 1012 } })
            .collect();
        let smt_l2: Vec<Option<u32>> = (0..24).map(|c| Some(if c < 16 { c / 2 } else { 8 + (c - 16) / 4 })).collect();
        let mixed_l2: Vec<Option<u32>> = (0..24).map(|c| (c % 7 != 3).then_some(c % 5)).collect();
        let shapes = [(real.v.clone(), real.l2.iter().map(|&l| Some(l)).collect::<Vec<_>>()), (smt, smt_l2), (mixed, mixed_l2)];
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for (shape, l2s) in &shapes {
            let n = shape.len();
            for _ in 0..4000 {
                let mut v = shape.clone();
                let idle_mask = next() & (next() | next());
                let online_mask = next() | next() | next();
                for (c, cv) in v.iter_mut().enumerate() {
                    cv.idle = idle_mask >> c & 1 != 0;
                    cv.load = usize::from(!cv.idle);
                }
                let f = |c: usize| if online_mask >> c & 1 != 0 { v.get(c).copied() } else { None };
                let l2 = |c: usize| l2s.get(c).copied().flatten();
                let prev = Some((next() % (n as u64 + 2)) as usize).filter(|&p| p < n);
                for pref in [Pref::Any, Pref::Performance, Pref::Efficiency] {
                    assert_eq!(hybrid_idle(n, &f, &l2, prev, pref), plain_hybrid_idle(n, &f, &l2, prev, pref), "prev {prev:?} {pref:?} idle {idle_mask:#x} online {online_mask:#x}");
                }
            }
        }
    }

    /// How many `view` and `l2` calls one wake-up takes, on (views, L2 domains).
    fn asked(v: &[CpuView], l2s: &[u32], prev: Option<usize>) -> usize {
        let calls = core::cell::Cell::new(0usize);
        let f = |c: usize| {
            calls.set(calls.get() + 1);
            v.get(c).copied()
        };
        let l2 = |c: usize| {
            calls.set(calls.get() + 1);
            l2s.get(c).copied()
        };
        hybrid_idle(v.len(), &f, &l2, prev, Pref::Any).expect("an idle CPU");
        calls.get()
    }

    /// A wake-up runs at interrupt time: the cost must grow with the CPUs, not with their square. Pinned on the real
    /// 265K (the plain walk took about 1,200 calls) and on a big hybrid CPU of 16 P and 112 E cores in clusters of four.
    #[test]
    fn hybrid_idle_asks_little() {
        let mut m = real_265k();
        let all_idle = asked(&m.v, &m.l2, None);
        assert!(all_idle <= 8 * m.v.len(), "all idle: {all_idle}");
        m.busy(0..8);
        let p_busy = asked(&m.v, &m.l2, Some(3));
        assert!(p_busy <= 8 * m.v.len(), "P busy: {p_busy}");
        m.busy([8, 12, 16]);
        let one_each = asked(&m.v, &m.l2, Some(3));
        assert!(one_each <= 8 * m.v.len(), "one busy in each E cluster: {one_each}");

        let n = 128u32;
        let mut big: Vec<CpuView> = (0..n)
            .map(|c| CpuView { idle: true, load: 0, core: (0, c), cluster: (0, 0), core_type: if c < 16 { P } else { E }, capacity: if c < 16 { 1024 } else { 802 } })
            .collect();
        let big_l2: Vec<u32> = (0..n).map(|c| if c < 16 { c } else { 16 + (c - 16) / 4 }).collect();
        let big_idle = asked(&big, &big_l2, None);
        assert!(big_idle <= 8 * big.len(), "big, all idle: {big_idle}");
        for x in big.iter_mut().take(16) {
            x.idle = false;
        }
        let big_p_busy = asked(&big, &big_l2, Some(3));
        assert!(big_p_busy <= 8 * big.len(), "big, P busy: {big_p_busy}");
        // Each cluster has to be counted here: its count is the question. Once per L2, not once per CPU.
        for c in (16..n as usize).step_by(4) {
            big[c].idle = false;
        }
        let big_clusters = asked(&big, &big_l2, Some(3));
        assert!(big_clusters <= 2 * 32 * big.len(), "big, one busy per cluster: {big_clusters}");
    }

    // The code before CP-265K, kept to hold a uniform CPU's answers to it.
    fn old_select(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, prev: Option<usize>, pref: Pref) -> Option<usize> {
        let hybrid = (0..n).filter_map(view).any(|v| v.core_type != U);
        let pv = prev.and_then(view);
        let near = |v: &CpuView| pv.is_none_or(|p| p.cluster == v.cluster);
        let mut best: Option<(usize, usize)> = None;
        for c in 0..n {
            let Some(v) = view(c) else { continue };
            if !v.idle {
                continue;
            }
            let score = type_penalty(pref, v.core_type, hybrid) * 16
                + usize::from(!near(&v)) * 8
                + usize::from(Some(c) != prev) * 4
                + usize::from(!core_idle(n, view, &v)) * 2
                + usize::from(v.capacity < 1024 && pref == Pref::Performance);
            if best.is_none_or(|(_, s)| score < s) {
                best = Some((c, score));
            }
        }
        if let Some((c, _)) = best {
            return Some(c);
        }
        let mut best: Option<(usize, usize)> = None;
        for c in 0..n {
            let Some(v) = view(c) else { continue };
            let score = v.load * 16 * 1024 / usize::from(v.capacity.max(1)) + type_penalty(pref, v.core_type, hybrid) * 8 + usize::from(!near(&v)) * 2 + usize::from(Some(c) != prev);
            if best.is_none_or(|(_, s)| score < s) {
                best = Some((c, score));
            }
        }
        best.map(|(c, _)| c)
    }

    fn old_push_target(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, from: usize) -> Option<usize> {
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

    fn old_steal_victim(n: usize, view: &dyn Fn(usize) -> Option<CpuView>, waiting: &dyn Fn(usize) -> usize, me: usize) -> Option<usize> {
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

    /// A uniform CPU (the Mini-PC's shape, and two CCX) answers exactly as before, over many states, and never asks
    /// for an L2 domain. A small generator is enough; the states need no quality, only variety.
    #[test]
    fn a_uniform_cpu_behaves_as_before() {
        let shapes: [Vec<CpuView>; 2] = [
            smt16(),
            (0..16).map(|c| CpuView { idle: false, load: 1, core: (0, c), cluster: (0, c / 4), core_type: U, capacity: 1024 }).collect(),
        ];
        let never = |c: usize| -> Option<u32> { panic!("L2 of CPU {c} asked on a uniform CPU") };
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let mut compared = 0;
        for shape in shapes {
            for _ in 0..4000 {
                let mut v = shape.clone();
                let idle_mask = next();
                let online_mask = next() | next();
                for (c, cv) in v.iter_mut().enumerate() {
                    cv.idle = idle_mask >> c & 1 != 0 && idle_mask >> (c + 16) & 1 != 0;
                    cv.load = if cv.idle { 0 } else { 1 + (next() % 5) as usize };
                }
                let f = |c: usize| if online_mask >> c & 1 != 0 { v.get(c).copied() } else { None };
                let w = next();
                let waiting = move |c: usize| ((w >> (c * 2)) & 3) as usize;
                let prev = Some((next() % 18) as usize).filter(|&p| p < 16);
                for pref in [Pref::Any, Pref::Performance, Pref::Efficiency] {
                    assert_eq!(select_by(16, &f, &never, prev, pref), old_select(16, &f, prev, pref));
                    compared += 1;
                }
                for c in 0..16 {
                    assert_eq!(push_target_by(16, &f, &never, c), old_push_target(16, &f, c));
                    assert_eq!(steal_victim_by(16, &f, &never, &waiting, c), old_steal_victim(16, &f, &waiting, c));
                }
            }
        }
        assert_eq!(compared, 24_000);
    }

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
        // CPUs 0-7 P cores, 8-19 E cores (no SMT on either).
        let mut v: Vec<CpuView> = (0..20)
            .map(|c| CpuView { idle: true, load: 0, core: (0, c), cluster: (0, if c < 8 { 0 } else { 1 + (c - 8) / 4 }), core_type: if c < 8 { P } else { E }, capacity: if c < 8 { 1024 } else { 802 } })
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
        // Favoured cores on a CPU that shows no core types: CPU 2 is the fastest (1024), the others 1012, CPU 3 slow.
        let caps = [1012u16, 1012, 1024, 802];
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
