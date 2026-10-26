//! The first page tables (4-level). All table pages come from one `BITS_PAGETABLE` pool allocated before
//! ExitBootServices, so the direct map can be completed from the final memory map without any firmware call.

use crate::mem::{self, PAGE};
use crate::{R, fail};
use bootinfo::layout::DIRECT_MAP_BASE;
use bootinfo::memtype;
use uefi::boot::AllocateType;

const P: u64 = 1;
const W: u64 = 1 << 1;
const PWT: u64 = 1 << 3;
const PS: u64 = 1 << 7;
const NX: u64 = 1 << 63;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
const MB2: u64 = 2 << 20;
const GB1: u64 = 1 << 30;
const GB512: u64 = 1 << 39;

#[derive(Clone, Copy)]
pub struct Perm {
    pub write: bool,
    pub exec: bool,
    /// PWT=1 PCD=0 selects PAT entry 1, which the kernel programs to write-combining.
    pub pat1: bool,
}
pub const DIRECT: Perm = Perm { write: true, exec: false, pat1: false };

pub struct Tables {
    pool: u64,
    pages: u64,
    used: u64,
    nx: u64,
    pub pml4: u64,
}

fn idx(virt: u64, shift: u32) -> u64 {
    (virt >> shift) & 511
}

fn slot(table: u64, i: u64) -> &'static mut u64 {
    // SAFETY: `table` is a page of the BITS_PAGETABLE pool (identity-mapped until the jump) and `i` < 512,
    // so the entry lies inside it; the loader runs on one CPU and holds one entry reference at a time.
    unsafe { &mut *((table + i * 8) as *mut u64) }
}

/// Upper bound of table pages for the RAM ranges plus kernel and framebuffer spans of the given sizes.
pub fn estimate(ram: impl Iterator<Item = (u64, u64)>, kernel: u64, fb: u64) -> u64 {
    // Per RAM range: a 4 KiB table at each unaligned end, the PDs and PDPTs it touches.
    let tables: u64 = ram.map(|(s, e)| 2 + (e - s) / GB1 + 2 + (e - s) / GB512 + 2).sum();
    let span = |bytes: u64| bytes / MB2 + 2 + bytes / GB1 + 2 + 1;
    // PML4, the stub's identity chain, and room for the second direct-map pass after ExitBootServices.
    1 + 3 + 16 + tables + span(kernel) + span(fb)
}

impl Tables {
    pub fn new(pages: u64, nx_ok: bool) -> R<Self> {
        let pool = mem::alloc(memtype::BITS_PAGETABLE, AllocateType::AnyPages, pages, "the page tables")?;
        mem::zero(pool, pages * PAGE);
        Ok(Tables { pool, pages, used: 1, nx: if nx_ok { NX } else { 0 }, pml4: pool })
    }

    fn alloc(&mut self) -> Option<u64> {
        (self.used < self.pages).then(|| {
            self.used += 1;
            self.pool + (self.used - 1) * PAGE
        })
    }

    /// The table below `table[i]`, created if missing; `None` if the pool is empty or a large page is there.
    fn next(&mut self, table: u64, i: u64) -> Option<u64> {
        let e = slot(table, i);
        if *e & P == 0 {
            let t = self.alloc()?;
            *e = t | P | W;
            return Some(t);
        }
        (*e & PS == 0).then_some(*e & ADDR)
    }

    fn leaf(&self, p: Perm) -> u64 {
        P | if p.write { W } else { 0 } | if p.pat1 { PWT } else { 0 } | if p.exec { 0 } else { self.nx }
    }

    pub fn map_4k(&mut self, virt: u64, phys: u64, p: Perm) -> Option<()> {
        let pdpt = self.next(self.pml4, idx(virt, 39))?;
        let pd = self.next(pdpt, idx(virt, 30))?;
        let pt = self.next(pd, idx(virt, 21))?;
        let e = slot(pt, idx(virt, 12));
        let mut bits = self.leaf(p);
        if *e & P != 0 {
            // Two ELF segments share this page: it gets the rights of both.
            bits |= *e & W;
            if *e & NX == 0 {
                bits &= !NX;
            }
        }
        *e = (phys & ADDR) | bits;
        Some(())
    }

    /// `None` if a 4 KiB table already covers this block (the caller then maps 4 KiB pages).
    fn map_2m(&mut self, virt: u64, phys: u64, p: Perm) -> Option<()> {
        let pdpt = self.next(self.pml4, idx(virt, 39))?;
        let pd = self.next(pdpt, idx(virt, 30))?;
        let e = slot(pd, idx(virt, 21));
        if *e & P != 0 && *e & PS == 0 {
            return None;
        }
        *e = (phys & ADDR) | self.leaf(p) | PS;
        Some(())
    }

    pub fn map_range(&mut self, virt: u64, phys: u64, size: u64, p: Perm, what: &str) -> R<()> {
        for off in (0..size).step_by(PAGE as usize) {
            self.map_4k(virt + off, phys + off, p).ok_or_else(|| fail!("page tables too small for {what}"))?;
        }
        Ok(())
    }

    /// Maps each RAM range at `DIRECT_MAP_BASE + phys`: 2 MiB pages where a whole aligned block is RAM.
    /// Returns the highest physical address covered. Mapping a range twice is harmless.
    pub fn map_direct(&mut self, ram: impl Iterator<Item = (u64, u64)>) -> Option<u64> {
        let mut top = 0;
        for (start, end) in ram {
            let mut a = start;
            while a < end {
                let virt = DIRECT_MAP_BASE + a;
                if a % MB2 == 0 && a + MB2 <= end && self.map_2m(virt, a, DIRECT).is_some() {
                    a += MB2;
                } else {
                    self.map_4k(virt, a, DIRECT)?;
                    a += PAGE;
                }
            }
            top = top.max(end);
        }
        Some(top)
    }

    pub fn used(&self) -> u64 {
        self.used
    }
}
