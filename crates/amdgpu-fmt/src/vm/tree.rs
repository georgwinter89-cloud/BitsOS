//! The CPU-side shadow of a VM's tables: which slots hold leaves, which hold child tables. Every hardware
//! write goes through here, so shadow and tables never disagree.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::collections::btree_map::Entry;
use alloc::vec::Vec;

use super::{Level, Mapping, VmError};
use crate::blocks::{self, Ent};
use crate::pte;
use crate::store::{ENTRIES, TableAddr, TableStore};

pub(super) struct Node {
    pub table: TableAddr,
    leaves: [u64; ENTRIES / 64],
    kids: BTreeMap<u16, Box<Node>>,
}

/// A table from the store whose address a PDE can hold.
pub(super) fn alloc_checked(store: &mut dyn TableStore) -> Result<TableAddr, VmError> {
    let t = store.alloc().ok_or(VmError::NoMemory)?;
    if t.phys % crate::GPU_PAGE != 0 || pte::pde(t).is_err() {
        store.free(t);
        return Err(VmError::BadTable(t.phys));
    }
    Ok(t)
}

fn child_of(level: Level) -> Level {
    match level {
        Level::Pdb2 => Level::Pdb1,
        Level::Pdb1 => Level::Pdb0,
        _ => Level::Ptb,
    }
}

impl Node {
    pub fn new(table: TableAddr) -> Node {
        Node { table, leaves: [0; ENTRIES / 64], kids: BTreeMap::new() }
    }

    fn leaf(&self, i: usize) -> bool {
        self.leaves[i / 64] & (1 << (i % 64)) != 0
    }
    fn mark(&mut self, i: usize, on: bool) {
        if on {
            self.leaves[i / 64] |= 1 << (i % 64);
        } else {
            self.leaves[i / 64] &= !(1 << (i % 64));
        }
    }
    fn is_empty(&self) -> bool {
        self.kids.is_empty() && self.leaves.iter().all(|&w| w == 0)
    }

    pub fn count(&self) -> usize {
        1 + self.kids.values().map(|k| k.count()).sum::<usize>()
    }

    /// Whether the table at `depth` (1 = PDB1 table .. 3 = PTB) on the way to `pfn` exists.
    fn has_table(&self, depth: usize, pfn: u64) -> bool {
        let mut node = self;
        for lvl in &Level::ALL[..depth] {
            match node.kids.get(&(lvl.index(pfn) as u16)) {
                Some(k) => node = k,
                None => return false,
            }
        }
        true
    }

    /// Tables the entries of `new` need that do not exist now (an upper bound when pieces are unsorted);
    /// `None` as soon as that passes `limit`. Bounded work: every entry visited lies in an existing table or
    /// in one of the at most `limit + 1` counted ones, so a hostile length costs at most 512 * (limit + 1 +
    /// existing tables) steps before it is refused.
    #[allow(clippy::needless_range_loop)]
    pub fn count_missing(&self, new: &[Mapping], limit: usize) -> Option<usize> {
        let mut last = [u64::MAX; 4];
        let mut n = 0;
        for m in new {
            for (pfn, frag) in m.blocks() {
                for en in blocks::entries(pfn, frag) {
                    for depth in 1..=en.level.depth() {
                        let id = en.pfn >> Level::ALL[depth - 1].shift();
                        if last[depth] == id {
                            continue;
                        }
                        last[depth] = id;
                        if !self.has_table(depth, en.pfn) {
                            n += 1;
                            if n > limit {
                                return None;
                            }
                        }
                    }
                }
            }
        }
        Some(n)
    }

    /// Clears the leaf of `en` if it is there.
    pub fn clear(&mut self, store: &mut dyn TableStore, en: Ent) {
        let mut node = self;
        for lvl in &Level::ALL[..en.level.depth()] {
            match node.kids.get_mut(&(lvl.index(en.pfn) as u16)) {
                Some(k) => node = k,
                None => return,
            }
        }
        let i = en.level.index(en.pfn);
        if node.leaf(i) {
            node.mark(i, false);
            store.write(node.table.phys, i, en.level.invalid());
        }
    }

    /// Writes leaf `value` for `en`, linking tables from `pool` on the way. False when the pool ran dry or a
    /// table address does not encode (both impossible after the commit's checks).
    pub fn set(
        &mut self,
        store: &mut dyn TableStore,
        pool: &mut Vec<TableAddr>,
        pending: &mut Vec<(u64, TableAddr)>,
        seq: u64,
        en: Ent,
        value: u64,
    ) -> bool {
        let mut node = self;
        for &lvl in &Level::ALL[..en.level.depth()] {
            let i = lvl.index(en.pfn);
            // An old leaf where a table must go: the PDE below replaces it in one store.
            node.mark(i, false);
            let key = i as u16;
            if let Entry::Vacant(e) = node.kids.entry(key) {
                let Some(t) = pool.pop() else { return false };
                let Ok(pde) = pte::pde(t) else { return false };
                store.fill(t.phys, child_of(lvl).invalid());
                e.insert(Box::new(Node::new(t)));
                store.write(node.table.phys, i, pde);
            }
            match node.kids.get_mut(&key) {
                Some(k) => node = k,
                None => return false,
            }
        }
        let i = en.level.index(en.pfn);
        if let Some(kid) = node.kids.remove(&(i as u16)) {
            (*kid).unlink(pending, seq);
        }
        node.mark(i, true);
        store.write(node.table.phys, i, value);
        true
    }

    /// Unlinks empty tables below this node (at `level`, first page `base`) that overlap pages `[s, e)`.
    #[allow(clippy::too_many_arguments)]
    pub fn prune(
        &mut self,
        store: &mut dyn TableStore,
        level: Level,
        base: u64,
        s: u64,
        e: u64,
        pending: &mut Vec<(u64, TableAddr)>,
        seq: u64,
    ) {
        let span = 1u64 << level.shift();
        let mut empty = Vec::new();
        for (&i, kid) in self.kids.iter_mut() {
            let kb = base + u64::from(i) * span;
            if kb >= e || kb + span <= s {
                continue;
            }
            kid.prune(store, child_of(level), kb, s, e, pending, seq);
            if kid.is_empty() {
                empty.push(i);
            }
        }
        for i in empty {
            if let Some(kid) = self.kids.remove(&i) {
                store.write(self.table.phys, usize::from(i), level.invalid());
                (*kid).unlink(pending, seq);
            }
        }
    }

    fn unlink(self, pending: &mut Vec<(u64, TableAddr)>, seq: u64) {
        let Node { table, kids, .. } = self;
        pending.push((seq, table));
        for (_, k) in kids {
            (*k).unlink(pending, seq);
        }
    }

    pub fn free_all(&mut self, store: &mut dyn TableStore) {
        for (_, mut k) in core::mem::take(&mut self.kids) {
            k.free_all(store);
        }
        store.free(self.table);
    }
}
