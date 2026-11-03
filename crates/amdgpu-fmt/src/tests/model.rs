//! Reference model of a VM's mappings (independent of the builder) and the checks that compare them.

use std::collections::BTreeMap;
use std::vec::Vec;

use super::{MemStore, Rng};
use crate::pte::{MapFlags, Target};
use crate::vm::{Backing, Kind, Vm};
use crate::walk::{self, Walk};
use crate::{GPU_PAGE, addr};

#[derive(Clone, Debug, PartialEq)]
pub enum MKind {
    Local(u64),
    System(Vec<u64>, bool),
    Prt,
}

#[derive(Clone, Debug)]
pub struct MMap {
    pub pages: u64,
    pub kind: MKind,
    pub flags: MapFlags,
}

#[derive(Debug, PartialEq)]
pub enum Expect {
    Page(u64, Target, MapFlags),
    Prt,
    Hole,
}

#[derive(Default)]
pub struct Model {
    /// start page (index space) -> mapping
    pub maps: BTreeMap<u64, MMap>,
}

impl Model {
    pub fn overlaps(&self, s: u64, e: u64) -> bool {
        self.maps.iter().any(|(&ms, m)| ms < e && s < ms + m.pages)
    }

    pub fn map(&mut self, s: u64, m: MMap) {
        self.maps.insert(s, m);
    }

    fn piece(m: &MMap, skip: u64, pages: u64) -> MMap {
        let kind = match &m.kind {
            MKind::Local(p) => MKind::Local(p + skip * GPU_PAGE),
            MKind::System(v, sn) => MKind::System(v[skip as usize..(skip + pages) as usize].to_vec(), *sn),
            MKind::Prt => MKind::Prt,
        };
        MMap { pages, kind, flags: m.flags }
    }

    pub fn clear(&mut self, s: u64, e: u64) {
        let hit: Vec<u64> =
            self.maps.iter().filter(|&(&ms, m)| ms < e && s < ms + m.pages).map(|(&k, _)| k).collect();
        for k in hit {
            let m = self.maps.remove(&k).unwrap();
            let me = k + m.pages;
            if k < s {
                self.maps.insert(k, Model::piece(&m, 0, s - k));
            }
            if me > e {
                self.maps.insert(e, Model::piece(&m, e - k, me - e));
            }
        }
    }

    pub fn expect(&self, idx: u64) -> Expect {
        let pfn = idx / GPU_PAGE;
        let Some((&s, m)) = self.maps.range(..=pfn).next_back() else { return Expect::Hole };
        if pfn >= s + m.pages {
            return Expect::Hole;
        }
        let off = pfn - s;
        match &m.kind {
            MKind::Local(p) => Expect::Page(p + off * GPU_PAGE + idx % GPU_PAGE, Target::Local, m.flags),
            MKind::System(v, sn) => {
                Expect::Page(v[off as usize] + idx % GPU_PAGE, Target::System { snooped: *sn }, m.flags)
            }
            MKind::Prt => Expect::Prt,
        }
    }
}

pub fn backing(k: &MKind) -> Backing<'_> {
    match k {
        MKind::Local(p) => Backing::Local { phys: *p },
        MKind::System(v, sn) => Backing::System { pages: v, snooped: *sn },
        MKind::Prt => Backing::Prt,
    }
}

fn got(w: Walk) -> Expect {
    match w {
        Walk::Page(l) => Expect::Page(l.phys, l.target, l.flags),
        Walk::Prt { .. } => Expect::Prt,
        Walk::Hole { .. } => Expect::Hole,
    }
}

/// VAs worth probing: edges of every mapping, around them, and random ones in `window` pages.
pub fn probes(model: &Model, rng: &mut Rng, window: u64) -> Vec<u64> {
    let mut v = Vec::new();
    for (&s, m) in &model.maps {
        let e = s + m.pages;
        for p in [s, e - 1, s + rng.below(m.pages), s.wrapping_sub(1), e] {
            if p < (1 << 36) {
                v.push(p * GPU_PAGE + rng.below(GPU_PAGE));
            }
        }
    }
    for _ in 0..16 {
        v.push(rng.below(window) * GPU_PAGE);
    }
    v
}

/// The builder agrees with the model, with a fresh build of the same mappings, and has no leaks.
/// Returns how often each leaf level (PDB1, PDB0, PTB) and PRT showed up in the probes.
pub fn check(vm: &Vm, store: &MemStore, model: &Model, rng: &mut Rng, window: u64) -> [usize; 4] {
    let mut seen = [0usize; 4];
    // Mappings as the builder records them equal the model.
    let mine: Vec<(u64, u64, MKind, MapFlags)> = vm
        .mappings()
        .map(|m| {
            let k = match &m.kind {
                Kind::Local { phys } => MKind::Local(*phys),
                Kind::System { pages, snooped } => MKind::System(pages.clone(), *snooped),
                Kind::Prt => MKind::Prt,
            };
            (m.start / GPU_PAGE, m.pages, k, m.flags)
        })
        .collect();
    let want: Vec<(u64, u64, MKind, MapFlags)> =
        model.maps.iter().map(|(&s, m)| (s, m.pages, m.kind.clone(), m.flags)).collect();
    assert_eq!(mine, want);
    assert_eq!(store.tables.len(), vm.table_count() + vm.pending_count(), "table leak");

    let mut fresh_store = MemStore::new();
    let mut fresh = Vm::new(&mut fresh_store, super::whole()).unwrap();
    for m in vm.mappings() {
        let b = match &m.kind {
            Kind::Local { phys } => Backing::Local { phys: *phys },
            Kind::System { pages, snooped } => Backing::System { pages, snooped: *snooped },
            Kind::Prt => Backing::Prt,
        };
        fresh.map(&mut fresh_store, addr::sign_extend(m.start), m.pages * GPU_PAGE, b, m.flags).unwrap();
    }
    assert_eq!(fresh.table_count(), vm.table_count(), "incremental tables differ from a fresh build");

    for idx in probes(model, rng, window) {
        let va = addr::sign_extend(idx);
        let w = walk::walk(store, vm.root().phys, va).unwrap();
        assert_eq!(got(w), model.expect(idx), "va {va:#x}");
        let f = walk::walk(&fresh_store, fresh.root().phys, va).unwrap();
        assert_eq!(w, f, "va {va:#x}: incremental and fresh tables differ");
        match w {
            Walk::Page(l) => seen[l.level as usize - 1] += 1,
            Walk::Prt { .. } => seen[3] += 1,
            Walk::Hole { .. } => {}
        }
        walk::check_fragment(store, vm.root().phys, va).unwrap();
    }
    fresh.destroy(&mut fresh_store);
    assert!(fresh_store.tables.is_empty());
    seen
}
