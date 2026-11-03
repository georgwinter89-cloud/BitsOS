//! How a mapping splits into fragment blocks and entries.
//!
//! A block is 2^frag pages, aligned in GPU VA and (for contiguous memory) in physical address, lying inside
//! the mapping: frag = min(trailing zeros of the VA page, of the physical page, floor(log2(pages left)), 31).
//! That is Linux's choice (amdgpu_vm_pt.c:860-918 amdgpu_vm_pte_fragment) plus the physical alignment Linux
//! gets from its VRAM allocator. Scattered system pages always get frag 0 (amdgpu_vm_pt.c:891).
//! A block of frag >= 18 is written as 1 GiB leaves on PDB1, frag >= 9 as 2 MiB leaves on PDB0, smaller
//! ones as PTEs (amdgpu_vm_pt.c:960-975 picks the level by the same rule); every entry of the block carries
//! the block's frag.

use crate::pte::MAX_FRAG;
use crate::vm::Level;

/// One entry to write: its level, the first VA page it covers, the block's fragment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ent {
    pub level: Level,
    pub pfn: u64,
    pub frag: u32,
}

/// Blocks of `[start, end)` (VA pages). `pa`: physical page of `start` for contiguous memory, `None` for PRT
/// (no physical side); `scattered`: frag 0 everywhere.
#[derive(Clone, Debug)]
pub(crate) struct Blocks {
    cur: u64,
    end: u64,
    delta: Option<u64>,
    scattered: bool,
}

impl Blocks {
    pub fn new(start: u64, end: u64, pa: Option<u64>, scattered: bool) -> Blocks {
        Blocks { cur: start, end, delta: pa.map(|p| p.wrapping_sub(start)), scattered }
    }
}

/// floor(log2(n)) for n > 0.
fn log2(n: u64) -> u32 {
    63 - n.leading_zeros()
}

impl Iterator for Blocks {
    /// (first page, frag)
    type Item = (u64, u32);
    fn next(&mut self) -> Option<(u64, u32)> {
        if self.cur >= self.end {
            return None;
        }
        let pfn = self.cur;
        let frag = if self.scattered {
            0
        } else {
            let mut f = pfn.trailing_zeros().min(log2(self.end - pfn)).min(MAX_FRAG);
            if let Some(d) = self.delta {
                f = f.min(pfn.wrapping_add(d).trailing_zeros());
            }
            f
        };
        self.cur = pfn + (1u64 << frag);
        Some((pfn, frag))
    }
}

/// The level a block of this fragment is written on.
pub(crate) fn level_for(frag: u32) -> Level {
    if frag >= Level::Pdb1.shift() {
        Level::Pdb1
    } else if frag >= Level::Pdb0.shift() {
        Level::Pdb0
    } else {
        Level::Ptb
    }
}

/// Entries of one block.
pub(crate) fn entries(pfn: u64, frag: u32) -> impl Iterator<Item = Ent> {
    let level = level_for(frag);
    let step = 1u64 << level.shift();
    let n = 1u64 << (frag - level.shift());
    (0..n).map(move |k| Ent { level, pfn: pfn + k * step, frag })
}
