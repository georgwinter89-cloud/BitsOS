//! GP-CPU2: placement with utilisation on the real 265K's numbers, and the uniform CPU unchanged.

use super::*;
use alloc::vec::Vec;
use hal::smp::{CORE_TYPE_EFFICIENCY as E, CORE_TYPE_PERFORMANCE as P, CORE_TYPE_UNIFORM as U};

/// The 265K: CPUs 0-7 P (3 and 5 favoured, 1024; the others 1012), 8-19 E (802) in L2 groups of four.
fn k265() -> (Vec<CpuView>, Vec<u32>) {
    let v = (0..20u32)
        .map(|c| CpuView {
            idle: true,
            load: 0,
            core: (0, c),
            cluster: (0, 0),
            core_type: if c < 8 { P } else { E },
            capacity: if c == 3 || c == 5 { 1024 } else if c < 8 { 1012 } else { 802 },
        })
        .collect();
    let l2 = (0..20u32).map(|c| if c < 8 { c } else { 8 + (c - 8) / 4 * 4 }).collect();
    (v, l2)
}

fn sel(v: &[CpuView], l2: &[u32], prev: Option<usize>, pref: Pref) -> Option<usize> {
    let f = |c: usize| v.get(c).copied();
    let l = |c: usize| l2.get(c).copied();
    select_by(v.len(), &f, &l, prev, pref)
}

fn busy(v: &mut [CpuView], cpus: impl IntoIterator<Item = usize>) {
    for c in cpus {
        v[c].idle = false;
        v[c].load = 1;
    }
}

/// A busy (heavy) Normal thread goes where Interactive goes: the favoured P cores first. A light one keeps an idle
/// previous core of either kind and otherwise takes an E core, so the P cores stay free.
#[test]
fn light_threads_keep_p_cores_free_busy_ones_rank_by_capacity() {
    let (mut v, l2) = k265();
    let heavy = crate::hybrid::pref_for(crate::Class::Normal, 900, 802);
    let light = crate::hybrid::pref_for(crate::Class::Normal, 50, 802);
    assert_eq!((heavy, light), (Pref::Performance, Pref::Light));
    assert_eq!(sel(&v, &l2, None, heavy), Some(3), "the first favoured core (highest HWP number)");
    assert_eq!(sel(&v, &l2, Some(12), heavy), Some(3), "up from its E core");
    assert_eq!(sel(&v, &l2, None, light), Some(8), "a new light thread: an E core");
    assert_eq!(sel(&v, &l2, Some(2), light), Some(2), "its idle previous P core: warm caches");
    assert_eq!(sel(&v, &l2, Some(13), light), Some(13));
    busy(&mut v, [2]);
    assert_eq!(sel(&v, &l2, Some(2), light), Some(8), "previous P core busy: an E core, not another P core");
    busy(&mut v, 8..20);
    assert_eq!(sel(&v, &l2, Some(13), light), Some(3), "every E core busy: a P core rather than waiting");
}

/// Two busy threads on the favoured cores, the rest by capacity: the busiest work lands on the highest-ranked idle
/// cores; E cores only when every P core is taken.
#[test]
fn busy_threads_fill_p_cores_by_rank() {
    let (mut v, l2) = k265();
    let mut order = Vec::new();
    for _ in 0..9 {
        let c = sel(&v, &l2, None, Pref::Performance).unwrap();
        order.push(c);
        busy(&mut v, [c]);
    }
    assert_eq!(order, [3, 5, 0, 1, 2, 4, 6, 7, 8]);
}

#[test]
fn misfit_target_is_the_fastest_idle_core() {
    let (mut v, _) = k265();
    let f = |v: &[CpuView], from| {
        let g = |c: usize| v.get(c).copied();
        misfit_target(v.len(), &g, from)
    };
    busy(&mut v, [9]);
    assert_eq!(f(&v, 9), Some(3), "from an E core: the first favoured P core");
    busy(&mut v, [3, 5]);
    assert_eq!(f(&v, 9), Some(0), "then the lowest-numbered other P core");
    assert_eq!(f(&v, 0), None, "from a P core: no core is clearly faster");
    busy(&mut v, 0..8);
    assert_eq!(f(&v, 9), None, "no idle P core: it stays");
}

#[test]
fn a_faster_idle_core_pulls_from_slower_ones_first() {
    let (mut v, l2) = k265();
    busy(&mut v, [0, 9]);
    let f = |c: usize| v.get(c).copied();
    let l = |c: usize| l2.get(c).copied();
    let one = |c: usize| usize::from(c == 0 || c == 9);
    assert_eq!(steal_victim_by(20, &f, &l, &one, 3), Some(9), "P core 3 takes the E core's waiting thread");
    let more_on_p = |c: usize| if c == 0 { 2 } else { usize::from(c == 9) };
    assert_eq!(steal_victim_by(20, &f, &l, &more_on_p, 3), Some(0), "one more waiting outweighs it");
    assert_eq!(steal_victim_by(20, &f, &l, &one, 8), Some(9), "an E core: its own L2 first, never pulled down by capacity");
}

/// On a uniform CPU `Light` is `Any` (whose answers a test above holds to the old code), and `misfit_target` never
/// answers: the Mini-PC's placement is unchanged.
#[test]
fn uniform_cpus_ignore_light_and_misfits() {
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let never = |_: usize| -> Option<u32> { panic!("no L2 on a uniform CPU") };
    for _ in 0..4000 {
        let (idle, online, loads) = (next(), next() | next(), next());
        let v: Vec<CpuView> = (0..16u32)
            .map(|c| CpuView {
                idle: idle >> c & 1 != 0,
                load: (loads >> (c * 2) & 3) as usize,
                core: (0, c / 2),
                cluster: (0, c / 8),
                core_type: U,
                capacity: 1024,
            })
            .collect();
        let f = |c: usize| v.get(c).copied().filter(|_| online >> c & 1 != 0);
        let prev = Some((next() % 16) as usize);
        assert_eq!(select_by(16, &f, &never, prev, Pref::Light), select_by(16, &f, &never, prev, Pref::Any));
        for from in 0..16 {
            assert_eq!(misfit_target(16, &f, from), None);
        }
    }
}
