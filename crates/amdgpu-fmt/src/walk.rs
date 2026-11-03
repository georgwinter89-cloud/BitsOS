//! Software page walker: translates a GPU VA through the tables like GFX 11's UTCL2 walker. It is the oracle
//! of the builder's tests and a decoder for tables read back from the GPU (fault diagnosis, hang dumps).
//!
//! Rules: an invalid entry ends the walk (a hole, or a PRT range when PRT is set on a leaf); PDE_PTE makes a
//! directory entry a leaf whose address is the base of a 2 MiB (PDB0) or 1 GiB (PDB1) block; the fragment
//! field is only a TLB hint and never changes the translation ([`check_fragment`] checks that the hint is
//! honest). Anything this crate never writes is reported, not guessed.

use crate::addr;
use crate::pte::{self, DecodeError, Entry, MapFlags, PDE_PTE, Target, VALID};
use crate::store::TableRead;
use crate::vm::Level;
use crate::{GPU_PAGE, GPU_PAGE_SHIFT};

/// A translated page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    /// Host-physical address of the byte at the VA.
    pub phys: u64,
    pub target: Target,
    pub flags: MapFlags,
    /// Level of the leaf entry (PTB, or PDB0/PDB1 for huge leaves).
    pub level: Level,
    pub frag: u32,
}

/// Result of a walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Walk {
    Page(Leaf),
    /// A PRT range (reads give zero, writes are dropped) ended the walk here.
    Prt { level: Level, frag: u32 },
    /// Nothing mapped: an invalid entry on this level (a fault).
    Hole { level: Level },
}

/// Tables that do not walk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkError {
    /// The VA lies in the canonical hole.
    NonCanonical(u64),
    /// A PDE points at memory that is not a known table.
    Unreadable { level: Level, table: u64 },
    /// An entry with bits GR never writes.
    Bad { level: Level, entry: u64 },
    /// A huge leaf whose address is not aligned to its span, or a leaf on the root.
    Misplaced { level: Level, entry: u64 },
}

/// Translates `va` through the tables under `root` (host-physical address of the root table).
pub fn walk<R: TableRead + ?Sized>(mem: &R, root: u64, va: u64) -> Result<Walk, WalkError> {
    let idx = addr::va_index(va).ok_or(WalkError::NonCanonical(va))?;
    let pfn = idx >> GPU_PAGE_SHIFT;
    let mut table = root;
    for level in Level::ALL {
        let e = mem.read(table, level.index(pfn)).ok_or(WalkError::Unreadable { level, table })?;
        let is_ptb = level == Level::Ptb;
        if !is_ptb && e & VALID == 0 && e & PDE_PTE == 0 {
            return Ok(Walk::Hole { level });
        }
        let bad = |_: DecodeError| WalkError::Bad { level, entry: e };
        match pte::decode(e, is_ptb).map_err(bad)? {
            Entry::Invalid => return Ok(Walk::Hole { level }),
            Entry::Prt { frag } => {
                if level == Level::Pdb2 {
                    return Err(WalkError::Misplaced { level, entry: e });
                }
                return Ok(Walk::Prt { level, frag });
            }
            Entry::Page { phys, target, flags, frag, .. } => {
                let span = level.span();
                if level == Level::Pdb2 || phys & (span - 1) != 0 {
                    return Err(WalkError::Misplaced { level, entry: e });
                }
                let phys = phys + (idx & (span - 1));
                return Ok(Walk::Page(Leaf { phys, target, flags, level, frag }));
            }
            Entry::Table(t) => table = t.phys,
        }
    }
    // decode() never returns a table on the PTB level.
    Err(WalkError::Bad { level: Level::Ptb, entry: 0 })
}

/// Why a fragment hint lies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FragError {
    Walk(WalkError),
    /// The page at this VA inside the fragment block is missing or differs (address, flags, frag).
    Mismatch(u64),
}

/// Checks the fragment hint of the leaf at `va`: the 2^frag-page block around it must be aligned in VA and
/// physical address and map contiguously with identical flags and frag. Samples the first page of every leaf
/// entry in the block (of every 2^-13th part when the block holds more entries, which only a frag the builder
/// never writes on that level gives) and its last page, so any block costs at most 8193 walks.
pub fn check_fragment<R: TableRead + ?Sized>(mem: &R, root: u64, va: u64) -> Result<(), FragError> {
    let here = match walk(mem, root, va).map_err(FragError::Walk)? {
        Walk::Page(l) => l,
        _ => return Ok(()),
    };
    let idx = va & addr::HOLE_MASK;
    let block = GPU_PAGE << here.frag;
    let block_va = idx & !(block - 1);
    let into = (idx & !(GPU_PAGE - 1)) - block_va;
    let Some(block_phys) = (here.phys & !(GPU_PAGE - 1)).checked_sub(into) else {
        return Err(FragError::Mismatch(va));
    };
    if block_phys & (block - 1) != 0 {
        return Err(FragError::Mismatch(va));
    }
    let step = here.level.span().max(block >> 13);
    let mut off = 0;
    while off < block {
        probe(mem, root, block_va + off, block_phys + off, &here)?;
        off += step;
    }
    probe(mem, root, block_va + block - GPU_PAGE, block_phys + block - GPU_PAGE, &here)
}

fn probe<R: TableRead + ?Sized>(mem: &R, root: u64, va: u64, phys: u64, want: &Leaf) -> Result<(), FragError> {
    match walk(mem, root, addr::sign_extend(va)).map_err(FragError::Walk)? {
        Walk::Page(l)
            if l.phys == phys
                && l.target == want.target
                && l.flags == want.flags
                && l.frag == want.frag
                && l.level == want.level =>
        {
            Ok(())
        }
        _ => Err(FragError::Mismatch(va)),
    }
}
