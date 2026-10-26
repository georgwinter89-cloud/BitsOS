//! Where an ELF's segments go (pure): the load bias, the page ranges with their final protection, the bytes to
//! copy from the file, the program break, the entry point and AT_PHDR. The address space work itself is in
//! `lib.rs`; this part is host-tested.
//!
//! Linux's rules (fs/binfmt_elf.c): every PT_LOAD maps `[PAGESTART(vaddr), PAGEALIGN(vaddr + memsz))`; the file
//! bytes start at `PAGESTART(offset)` (so the page holds what precedes the segment in the file too, e.g. the ELF
//! header in the first page); a page shared by two segments ends up with the later one's protection and
//! content (MAP_FIXED replaces it); bss is zero. ET_EXEC loads where it says (never below `mmap_min_addr`).
//! ET_DYN without PT_INTERP (static-PIE) is placed like any `mmap` without a usable hint: top-down below the
//! mmap base (`load_bias` 0, then `elf_map` picks the address), aligned down to the largest segment alignment
//! when that is above a page; its break starts at `PAGE_ALIGN(ELF_ET_DYN_BASE)` (Linux 6.10 "Move brk for
//! static PIE even if ASLR disabled"). Checked on CachyOS 7.2.9 with `setarch -R` on a static-PIE program: the
//! image ends at the mmap base 0x7ffff7fff000, the break is at 0x555555555000. No randomisation yet (open point
//! in plan/notes/ab-AB-2.md).
//!
//! AB-8, dynamic programs (`load_elf_binary` :1105-1137): an ET_DYN with PT_INTERP loads at `ELF_ET_DYN_BASE`
//! aligned down to its largest segment alignment, less its first segment's page; its break follows its image.
//! The interpreter itself is a static-PIE for this module (top-down below the mmap base). `segs` is the same
//! image as file-backed mappings (`elf_map` per segment, `padzero` of the last file page, the bss anonymous).

use crate::elf::{Elf, MIN_ADDR, PAGE, PF_R, PF_W, PF_X};
use alloc::vec::Vec;
use mm::Prot;

/// Linux's `ELF_ET_DYN_BASE` on x86-64 (`DEFAULT_MAP_WINDOW / 3 * 2`). Where a static-PIE's break starts (page
/// aligned up); AB-8's interpreter case loads the program here (aligned down to its segment alignment).
pub const ELF_ET_DYN_BASE: u64 = 0x0000_5555_5555_4AAA;
/// The first break of a static-PIE.
pub const PIE_BRK: u64 = ELF_ET_DYN_BASE.next_multiple_of(PAGE);

/// One page-aligned range of the image with its final protection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub start: u64,
    pub end: u64,
    pub prot: Prot,
}

/// File bytes `[src, src + len)` go to `dst` (in segment order; a later copy wins on a shared page).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CopyOp {
    pub dst: u64,
    pub src: u64,
    pub len: u64,
}

/// One PT_LOAD as mappings: `[start, file_end)` maps the file from `off` (page-aligned), `[file_end, end)` is
/// anonymous zero memory; `zero` is the part of the last file page past the segment's file bytes that is
/// cleared (only in a writable segment: Linux's `clear_user` fails silently on a read-only one).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub start: u64,
    pub file_end: u64,
    pub end: u64,
    pub off: u64,
    pub prot: Prot,
    pub zero: Option<(u64, u64)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub bias: u64,
    pub regions: Vec<Region>,
    pub copies: Vec<CopyOp>,
    /// The same image as file mappings, in segment order (a later one replaces a shared page, MAP_FIXED).
    pub segs: Vec<Seg>,
    /// Lowest and end address of the image.
    pub start: u64,
    pub end: u64,
    pub entry: u64,
    /// AT_PHDR.
    pub phdr: u64,
    /// Start of the heap: the page after the highest segment (ET_EXEC), `PIE_BRK` (static-PIE).
    pub brk: u64,
}

pub fn prot_of(flags: u32) -> Prot {
    Prot { read: flags & PF_R != 0, write: flags & PF_W != 0, exec: flags & PF_X != 0 }
}

/// Puts `r` into `v` (sorted, disjoint), cutting away what it covers of earlier regions.
fn insert(v: &mut Vec<Region>, r: Region) -> Result<(), &'static str> {
    let mut out: Vec<Region> = Vec::new();
    out.try_reserve(v.len() + 2).map_err(|_| "no memory for the load plan")?;
    for o in v.iter() {
        if o.end <= r.start || o.start >= r.end {
            out.push(*o);
            continue;
        }
        if o.start < r.start {
            out.push(Region { start: o.start, end: r.start, prot: o.prot });
        }
        if o.end > r.end {
            out.push(Region { start: r.end, end: o.end, prot: o.prot });
        }
    }
    out.push(r);
    out.sort_by_key(|x| x.start);
    *v = out;
    Ok(())
}

/// The load plan for `elf`. A static-PIE (and an interpreter) goes top-down below `mmap_top` (the address
/// space is still empty), a dynamic PIE to `ELF_ET_DYN_BASE`; the image must end at or below `limit` (where the
/// stack area starts).
pub fn plan(elf: &Elf, mmap_top: u64, limit: u64) -> Result<Plan, &'static str> {
    let (lo, hi) = elf.span();
    let len = hi.checked_sub(lo).ok_or("image outside user space")?;
    let start = if elf.is_pie && elf.interp.is_some() {
        // `load_bias = ELF_PAGESTART(ELF_ET_DYN_BASE & ~(alignment - 1) - first p_vaddr)`.
        let first = elf.loads.first().map_or(0, |l| l.vaddr);
        let bias = ((ELF_ET_DYN_BASE & !(elf.max_align - 1)).wrapping_sub(first)) & !(PAGE - 1);
        bias.wrapping_add(lo)
    } else if elf.is_pie {
        let top = mmap_top.min(limit);
        let s = top.checked_sub(len).ok_or("image larger than user space")?;
        if elf.max_align > PAGE { s & !(elf.max_align - 1) } else { s }
    } else {
        lo
    };
    // The bias may be "negative" (a PIE linked above where it lands): all sums below wrap like Linux's.
    let bias = start.wrapping_sub(lo);
    let end = start.checked_add(len).ok_or("image outside user space")?;
    if start < MIN_ADDR {
        return Err("segment below 64 KiB (mmap_min_addr)");
    }
    if end > limit {
        return Err("image reaches into the stack area");
    }
    let mut regions = Vec::new();
    let mut copies = Vec::new();
    copies.try_reserve(elf.loads.len()).map_err(|_| "no memory for the load plan")?;
    let mut segs = Vec::new();
    segs.try_reserve(elf.loads.len()).map_err(|_| "no memory for the load plan")?;
    for l in &elf.loads {
        let s = (l.vaddr & !(PAGE - 1)).wrapping_add(bias);
        let e = (l.vaddr + l.memsz).next_multiple_of(PAGE).wrapping_add(bias);
        let prot = prot_of(l.flags);
        insert(&mut regions, Region { start: s, end: e, prot })?;
        let lead = l.vaddr % PAGE;
        if l.filesz > 0 {
            copies.push(CopyOp { dst: s, src: l.offset - lead, len: l.filesz + lead });
        }
        let file_end = if l.filesz == 0 { s } else { (l.vaddr + l.filesz).next_multiple_of(PAGE).wrapping_add(bias) };
        let tail = (l.vaddr + l.filesz).wrapping_add(bias);
        let zero = (l.memsz > l.filesz && l.filesz > 0 && tail < file_end && prot.write).then_some((tail, file_end));
        segs.push(Seg { start: s, file_end, end: e, off: l.offset - lead, prot, zero });
    }
    let brk = if elf.is_pie && elf.interp.is_none() { PIE_BRK } else { end };
    Ok(Plan { bias, regions, copies, segs, start, end, entry: elf.entry.wrapping_add(bias), phdr: elf.phdr_vaddr.wrapping_add(bias), brk })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::{ET_DYN, ET_EXEC, PT_INTERP, PT_LOAD, build_test_elf};

    const TOP: u64 = 0x0000_7FFF_FFFF_F000;
    const LIMIT: u64 = TOP - (8 << 20);
    const MMAP: u64 = 0x0000_7F00_0000_0000;
    const RX: Prot = Prot { read: true, write: false, exec: true };
    const RW: Prot = Prot { read: true, write: true, exec: false };
    const R: Prot = Prot { read: true, write: false, exec: false };

    fn elf(ty: u16, entry: u64, phs: &[(u32, u32, u64, u64, u64, u64, u64)], size: usize) -> Elf {
        Elf::parse(&build_test_elf(ty, entry, phs, size), TOP).unwrap()
    }

    #[test]
    fn static_exec_like_gnu_ld() {
        // text at 0x400000 (headers inside), data+bss at 0x601e10 sharing the file page with text.
        let e = elf(ET_EXEC, 0x400100, &[(PT_LOAD, PF_R | PF_X, 0, 0x400000, 0x1e10, 0x1e10, 0x200000), (PT_LOAD, PF_R | PF_W, 0x1e10, 0x601e10, 0x200, 0x5000, 0x200000)], 0x2010);
        let p = plan(&e, MMAP, LIMIT).unwrap();
        assert_eq!(p.bias, 0);
        assert_eq!(p.entry, 0x400100);
        assert_eq!(p.phdr, 0x400040);
        assert_eq!(p.regions, [Region { start: 0x400000, end: 0x402000, prot: RX }, Region { start: 0x601000, end: 0x607000, prot: RW }]);
        assert_eq!(p.copies, [CopyOp { dst: 0x400000, src: 0, len: 0x1e10 }, CopyOp { dst: 0x601000, src: 0x1000, len: 0x1010 }]);
        assert_eq!(p.brk, 0x607000);
    }

    #[test]
    fn static_pie_like_lld() {
        // R (headers), RX, RW at consecutive pages from 0, as lld/rcrt1 lay out a static-PIE.
        let e = elf(ET_DYN, 0x1230, &[(PT_LOAD, PF_R, 0, 0, 0x500, 0x500, 0x1000), (PT_LOAD, PF_R | PF_X, 0x500, 0x1500, 0x300, 0x300, 0x1000), (PT_LOAD, PF_R | PF_W, 0x800, 0x2800, 0x100, 0x900, 0x1000)], 0x900);
        let p = plan(&e, MMAP, LIMIT).unwrap();
        // Top-down below the mmap base, like `setarch -R` on Linux: the image (4 pages) ends at the base.
        let base = MMAP - 0x4000;
        assert_eq!(p.bias, base);
        assert_eq!(p.entry, base + 0x1230);
        assert_eq!(p.phdr, base + 0x40);
        assert_eq!(p.regions.len(), 3);
        assert_eq!(p.regions[1], Region { start: base + 0x1000, end: base + 0x2000, prot: RX });
        assert_eq!(p.copies[2], CopyOp { dst: base + 0x2000, src: 0, len: 0x900 });
        assert_eq!(p.brk, PIE_BRK);
        assert_eq!(PIE_BRK, 0x5555_5555_5000, "what CachyOS 7.2.9 shows under setarch -R");
    }

    #[test]
    fn linux_mmap_base_gives_linux_addresses() {
        // CachyOS 7.2.9, setarch -R, a glibc static-PIE of 0xd9000 bytes: mapped at 0x7ffff7f26000.
        let e = elf(ET_DYN, 0x98e0, &[(PT_LOAD, PF_R, 0, 0, 0x7000, 0x7000, 0x1000), (PT_LOAD, PF_R | PF_W, 0xd0000, 0xd1000, 0x2000, 0x8000, 0x1000)], 0xd2000);
        let p = plan(&e, 0x7fff_f7ff_f000, LIMIT).unwrap();
        assert_eq!(p.bias, 0x7fff_f7f2_6000);
        assert_eq!(p.entry, 0x7fff_f7f2_f8e0);
    }

    #[test]
    fn dynamic_pie_goes_to_et_dyn_base() {
        // A gcc -pie program on CachyOS (`setarch -R`): text from 0x555555554000, the break after the bss.
        let mut b = build_test_elf(ET_DYN, 0x1040, &[(PT_INTERP, PF_R, 0x318, 0x318, 0x1c, 0x1c, 1), (PT_LOAD, PF_R, 0, 0, 0x6a8, 0x6a8, 0x1000), (PT_LOAD, PF_R | PF_X, 0x1000, 0x1000, 0x1a1, 0x1a1, 0x1000), (PT_LOAD, PF_R | PF_W, 0x2de8, 0x3de8, 0x248, 0x1250, 0x1000)], 0x3100);
        b[0x318..0x318 + 0x1c].copy_from_slice(b"/lib64/ld-linux-x86-64.so.2\0");
        let e = Elf::parse(&b, TOP).unwrap();
        assert_eq!(e.interp.as_deref(), Some(&b"/lib64/ld-linux-x86-64.so.2"[..]));
        let p = plan(&e, MMAP, LIMIT).unwrap();
        assert_eq!(p.bias, 0x5555_5555_4000);
        assert_eq!(p.entry, 0x5555_5555_5040);
        assert_eq!(p.phdr, 0x5555_5555_4040);
        assert_eq!((p.start, p.end), (0x5555_5555_4000, 0x5555_5555_A000));
        assert_eq!(p.brk, 0x5555_5555_A000, "the break follows the image (no static-PIE rule)");
        // The data segment: file pages 0x557000..0x559000 from offset 0x2000, its last file page cleared from
        // 0x558030 (= 0x3de8 + 0x248), then one anonymous page of bss.
        assert_eq!(p.segs[2], Seg { start: 0x5555_5555_7000, file_end: 0x5555_5555_9000, end: 0x5555_5555_A000, off: 0x2000, prot: RW, zero: Some((0x5555_5555_8030, 0x5555_5555_9000)) });
        assert_eq!(p.segs[0], Seg { start: 0x5555_5555_4000, file_end: 0x5555_5555_5000, end: 0x5555_5555_5000, off: 0, prot: R, zero: None });
        // Large alignment: the base aligns down first (Linux `maximum_alignment`).
        let e = elf(ET_DYN, 0, &[(PT_LOAD, PF_R | PF_X, 0, 0, 0x100, 0x100, 0x20_0000)], 0x100);
        let mut e2 = e.clone();
        e2.interp = Some(b"/l".to_vec());
        assert_eq!(plan(&e2, MMAP, LIMIT).unwrap().bias, 0x5555_5540_0000);
    }

    #[test]
    fn bss_segments_split_into_file_and_anonymous() {
        // Read-only segment with memsz > filesz: no clearing (Linux's clear_user fails on it and is ignored).
        let e = elf(ET_EXEC, 0x400000, &[(PT_LOAD, PF_R, 0, 0x400000, 0x100, 0x3000, 0x1000), (PT_LOAD, PF_R | PF_W, 0x1000, 0x404000, 0, 0x2000, 0x1000), (PT_LOAD, PF_R | PF_W, 0x1000, 0x407000, 0x1000, 0x2000, 0x1000)], 0x2000);
        let p = plan(&e, MMAP, LIMIT).unwrap();
        assert_eq!(p.segs[0], Seg { start: 0x400000, file_end: 0x401000, end: 0x403000, off: 0, prot: R, zero: None });
        assert_eq!(p.segs[1], Seg { start: 0x404000, file_end: 0x404000, end: 0x406000, off: 0x1000, prot: RW, zero: None }, "bss only");
        assert_eq!(p.segs[2], Seg { start: 0x407000, file_end: 0x408000, end: 0x409000, off: 0x1000, prot: RW, zero: None }, "file part ends on a page");
    }

    #[test]
    fn large_alignment_aligns_down() {
        let e = elf(ET_DYN, 0, &[(PT_LOAD, PF_R | PF_X, 0, 0, 0x100, 0x100, 0x20_0000)], 0x100);
        let p = plan(&e, MMAP, LIMIT).unwrap();
        assert_eq!(p.bias % 0x20_0000, 0);
        assert!(p.bias < MMAP && MMAP - p.bias <= 0x20_0000);
    }

    #[test]
    fn shared_page_takes_the_later_segment() {
        let e = elf(ET_EXEC, 0x400000, &[(PT_LOAD, PF_R | PF_X, 0, 0x400000, 0x1800, 0x1800, 0x1000), (PT_LOAD, PF_R, 0x1800, 0x401800, 0x100, 0x100, 0x1000)], 0x1900);
        let p = plan(&e, MMAP, LIMIT).unwrap();
        assert_eq!(p.regions, [Region { start: 0x400000, end: 0x401000, prot: RX }, Region { start: 0x401000, end: 0x402000, prot: R }]);
        // The second copy brings the page's leading file bytes too (they are the same bytes as the first's).
        assert_eq!(p.copies[1], CopyOp { dst: 0x401000, src: 0x1000, len: 0x900 });
    }

    #[test]
    fn refusals() {
        let low = elf(ET_EXEC, 0x1000, &[(PT_LOAD, PF_R | PF_X, 0, 0x1000, 0x100, 0x100, 0x1000)], 0x100);
        assert_eq!(plan(&low, MMAP, LIMIT).err(), Some("segment below 64 KiB (mmap_min_addr)"));
        let high = elf(ET_EXEC, 0, &[(PT_LOAD, PF_R, 0, LIMIT - 0x1000, 0x100, 0x2000, 0x1000)], 0x100);
        assert_eq!(plan(&high, MMAP, LIMIT).err(), Some("image reaches into the stack area"));
        // A PIE linked high still goes below the mmap base (Linux ignores its addresses: load_bias 0, no hint).
        let pie = elf(ET_DYN, 0x6000_0000_0010, &[(PT_LOAD, PF_R, 0, 0x6000_0000_0000, 0x100, 0x100, 0x1000)], 0x100);
        let p = plan(&pie, MMAP, LIMIT).unwrap();
        assert_eq!((p.regions[0].start, p.entry), (MMAP - 0x1000, MMAP - 0x1000 + 0x10));
        let up = elf(ET_DYN, 0x7f80_0000_0000, &[(PT_LOAD, PF_R, 0, 0x7f80_0000_0000, 0x100, 0x100, 0x1000)], 0x100);
        let p = plan(&up, MMAP, LIMIT).unwrap();
        assert_eq!((p.regions[0].start, p.entry), (MMAP - 0x1000, MMAP - 0x1000), "a negative bias wraps");
        let huge = elf(ET_DYN, 0, &[(PT_LOAD, PF_R, 0, 0, 0x100, 0x7F00_0000_0000, 0x1000)], 0x100);
        assert!(plan(&huge, MMAP, LIMIT).is_err());
    }
}
