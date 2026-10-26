//! The new kernel's first page tables, built as an image in memory (pure) and then copied into frames the
//! running kernel gives Reload. Same shape as bitsboot's (`boot/bitsboot/src/paging.rs`), so the new kernel
//! finds exactly the entry contract a cold boot gives it: the kernel span at its ELF addresses (4 KiB pages, W
//! only for PF_W, NX unless PF_X), every RAM range at `DIRECT_MAP_BASE + phys` (2 MiB pages where a whole aligned
//! block is RAM, write-back, NX), the framebuffer at `VMAP_BASE` (PWT: PAT entry 1 = WC once the kernel set the
//! PAT), and the stub page identity-mapped, executable.

use alloc::vec;
use alloc::vec::Vec;
use bootinfo::layout::DIRECT_MAP_BASE;

const P: u64 = 1;
const W: u64 = 1 << 1;
const PWT: u64 = 1 << 3;
const PS: u64 = 1 << 7;
const NX: u64 = 1 << 63;
const ADDR: u64 = 0x000F_FFFF_FFFF_F000;
const PAGE: u64 = 4096;
const MB2: u64 = 2 << 20;
const GB1: u64 = 1 << 30;
const GB512: u64 = 1 << 39;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perm {
    pub write: bool,
    pub exec: bool,
    /// PWT=1 PCD=0: PAT entry 1 (write-combining once the kernel programmed the PAT).
    pub pat1: bool,
}
pub const DIRECT: Perm = Perm { write: true, exec: false, pat1: false };

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// Table pages, all at known physical addresses `base + i * 4096` (one contiguous block).
pub struct Tables {
    base: u64,
    limit: usize,
    nx: u64,
    pages: Vec<[u64; 512]>,
}

/// Upper bound of table pages (bitsboot's estimate): per RAM range its unaligned ends and the PDs/PDPTs it
/// touches, plus the kernel and framebuffer spans, the PML4 and the stub's chain.
pub fn estimate(ram: &[(u64, u64)], kernel: u64, fb: u64) -> u64 {
    let tables: u64 = ram.iter().map(|&(s, e)| 2 + (e - s) / GB1 + 2 + (e - s) / GB512 + 2).sum();
    let span = |bytes: u64| bytes / MB2 + 2 + bytes / GB1 + 2 + 1;
    1 + 3 + tables + span(kernel) + span(fb)
}

fn idx(virt: u64, shift: u32) -> usize {
    ((virt >> shift) & 511) as usize
}

impl Tables {
    /// `limit` table pages at `base` (the PML4 is the first).
    pub fn new(base: u64, limit: usize, nx: bool) -> Self {
        Tables { base, limit: limit.max(1), nx: if nx { NX } else { 0 }, pages: vec![[0u64; 512]] }
    }

    pub fn pml4(&self) -> u64 {
        self.base
    }
    pub fn used(&self) -> usize {
        self.pages.len()
    }
    /// The table pages in order, page `i` at `pml4() + i * 4096`.
    pub fn pages(&self) -> &[[u64; 512]] {
        &self.pages
    }

    fn index_of(&self, phys: u64) -> Option<usize> {
        let i = (phys.checked_sub(self.base)? / PAGE) as usize;
        (i < self.pages.len()).then_some(i)
    }

    fn next(&mut self, t: usize, i: usize) -> Result<Option<usize>, Full> {
        let e = self.pages[t][i];
        if e & P == 0 {
            if self.pages.len() >= self.limit {
                return Err(Full);
            }
            self.pages.push([0; 512]);
            let n = self.pages.len() - 1;
            self.pages[t][i] = (self.base + n as u64 * PAGE) | P | W;
            return Ok(Some(n));
        }
        if e & PS != 0 {
            return Ok(None);
        }
        Ok(self.index_of(e & ADDR))
    }

    fn leaf(&self, p: Perm) -> u64 {
        P | if p.write { W } else { 0 } | if p.pat1 { PWT } else { 0 } | if p.exec { 0 } else { self.nx }
    }

    pub fn map_4k(&mut self, virt: u64, phys: u64, p: Perm) -> Result<(), Full> {
        let pdpt = self.next(0, idx(virt, 39))?.ok_or(Full)?;
        let pd = self.next(pdpt, idx(virt, 30))?.ok_or(Full)?;
        let pt = self.next(pd, idx(virt, 21))?.ok_or(Full)?;
        let mut bits = self.leaf(p);
        let e = &mut self.pages[pt][idx(virt, 12)];
        if *e & P != 0 {
            // Two segments share this page: it gets the rights of both.
            bits |= *e & W;
            if *e & NX == 0 {
                bits &= !NX;
            }
        }
        *e = (phys & ADDR) | bits;
        Ok(())
    }

    /// `Ok(false)` if a 4 KiB table already covers this block (the caller maps 4 KiB pages then).
    fn map_2m(&mut self, virt: u64, phys: u64, p: Perm) -> Result<bool, Full> {
        let pdpt = self.next(0, idx(virt, 39))?.ok_or(Full)?;
        let Some(pd) = self.next(pdpt, idx(virt, 30))? else { return Ok(false) };
        let leaf = self.leaf(p) | PS;
        let e = &mut self.pages[pd][idx(virt, 21)];
        if *e & P != 0 && *e & PS == 0 {
            return Ok(false);
        }
        *e = (phys & ADDR) | leaf;
        Ok(true)
    }

    pub fn map_range(&mut self, virt: u64, phys: u64, size: u64, p: Perm) -> Result<(), Full> {
        for off in (0..size).step_by(PAGE as usize) {
            self.map_4k(virt + off, phys + off, p)?;
        }
        Ok(())
    }

    /// Every RAM range at `DIRECT_MAP_BASE + phys`. Returns the highest physical address covered.
    pub fn map_direct(&mut self, ram: &[(u64, u64)]) -> Result<u64, Full> {
        let mut top = 0;
        for &(start, end) in ram {
            let mut a = start & !(PAGE - 1);
            while a < end {
                let virt = DIRECT_MAP_BASE + a;
                if a.is_multiple_of(MB2) && a + MB2 <= end && self.map_2m(virt, a, DIRECT)? {
                    a += MB2;
                } else {
                    self.map_4k(virt, a, DIRECT)?;
                    a += PAGE;
                }
            }
            top = top.max(end);
        }
        Ok(top)
    }

    /// Walks the image like the MMU: the physical address and (writable, executable, pat1) for `virt`.
    pub fn translate(&self, virt: u64) -> Option<(u64, Perm)> {
        let mut t = 0usize;
        let mut nx = false;
        for (level, shift) in [(4u32, 39u32), (3, 30), (2, 21), (1, 12)] {
            let e = self.pages[t][idx(virt, shift)];
            if e & P == 0 {
                return None;
            }
            nx |= e & NX != 0;
            if level == 1 || (level == 2 && e & PS != 0) {
                let size = 1u64 << shift;
                let phys = (e & ADDR & !(size - 1)) + (virt & (size - 1));
                return Some((phys, Perm { write: e & W != 0, exec: !nx, pat1: e & PWT != 0 }));
            }
            t = self.index_of(e & ADDR)?;
        }
        None
    }

    /// The table pages as bytes (little endian), page `i` at offset `i * 4096`.
    pub fn bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.pages.len() * PAGE as usize);
        for p in &self.pages {
            for e in p {
                v.extend_from_slice(&e.to_le_bytes());
            }
        }
        v
    }
}
