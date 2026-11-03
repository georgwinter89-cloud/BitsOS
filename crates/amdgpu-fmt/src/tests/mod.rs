//! Host tests: encoders against Linux's bit values, the builder against the walker and a reference model.

mod codec;
mod fences;
mod gart;
mod golden;
mod hangdump;
mod hostile;
mod hostile_rings;
mod ids;
mod model;
mod prop;
mod pte;
mod recovery;
mod recovery_chip;
mod recovery_hostile;
mod rings;
mod sim;
mod vm;
mod watch;

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::pte::Target;
use crate::store::{ENTRIES, TableAddr, TableRead, TableStore};

/// Generous interpreter limits for tests that are not about the limits.
pub fn limits() -> crate::sim::Limits {
    crate::sim::Limits { max_dw: 1 << 20, max_bytes: 1 << 24, max_ib_depth: 2 }
}

/// A carve-out spanning all 48 bits, for tests that are not about the carve-out bound.
pub fn whole() -> crate::addr::Carveout {
    crate::addr::Carveout::new(0, 0, crate::addr::VA_SIZE).unwrap()
}

/// xorshift64*: fixed seeds, no crate download needed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// 0..n (n > 0)
    pub fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }
}

/// Tables in plain memory with fake physical addresses, a write log and fault injection.
pub struct MemStore {
    pub tables: BTreeMap<u64, Vec<u64>>,
    next: u64,
    free_list: Vec<u64>,
    pub target: Target,
    /// Allocations left before `alloc` fails (`None`: never).
    pub fail_after: Option<usize>,
    /// Address the next allocation returns instead of a good one (hostile store).
    pub bad_next: Option<u64>,
    pub log: Vec<(u64, usize, u64)>,
    pub logging: bool,
    /// What `available` reports (`None`: unlimited); separate from `fail_after` so a store can also fail
    /// part way through a commit's allocations.
    pub capacity: Option<usize>,
}

impl MemStore {
    pub fn new() -> MemStore {
        MemStore::at(0x8_1000_0000, Target::Local)
    }
    pub fn at(base: u64, target: Target) -> MemStore {
        MemStore {
            tables: BTreeMap::new(),
            next: base,
            free_list: Vec::new(),
            target,
            fail_after: None,
            bad_next: None,
            log: Vec::new(),
            logging: false,
            capacity: None,
        }
    }
}

impl TableRead for MemStore {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        self.tables.get(&table)?.get(index).copied()
    }
}

impl TableStore for MemStore {
    fn alloc(&mut self) -> Option<TableAddr> {
        if let Some(n) = self.fail_after.as_mut() {
            if *n == 0 {
                return None;
            }
            *n -= 1;
        }
        if let Some(bad) = self.bad_next.take() {
            self.tables.insert(bad, std::vec![0; ENTRIES]);
            return Some(TableAddr { phys: bad, target: self.target });
        }
        let phys = self.free_list.pop().unwrap_or_else(|| {
            let p = self.next;
            self.next += crate::store::TABLE_BYTES;
            p
        });
        // Garbage, so a table the builder forgets to fill shows up in the walk.
        assert!(self.tables.insert(phys, std::vec![0xDEAD_BEEF_0000_0FFF; ENTRIES]).is_none());
        Some(TableAddr { phys, target: self.target })
    }

    fn available(&self) -> usize {
        self.capacity.map_or(usize::MAX, |c| c.saturating_sub(self.tables.len()))
    }

    fn free(&mut self, t: TableAddr) {
        assert!(self.tables.remove(&t.phys).is_some(), "free of unknown table {:#x}", t.phys);
        if t.phys.is_multiple_of(crate::GPU_PAGE) && t.phys >> 48 == 0 {
            self.free_list.push(t.phys);
        }
    }

    fn write(&mut self, table: u64, index: usize, value: u64) {
        let t = self.tables.get_mut(&table).unwrap_or_else(|| panic!("write to freed table {table:#x}"));
        t[index] = value;
        if self.logging {
            self.log.push((table, index, value));
        }
    }
}
