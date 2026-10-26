//! The kernel ELF check: header and program headers only, every value checked before use. A file that fails
//! here is never copied anywhere; the message names the first problem in words.

use alloc::vec::Vec;
use bootinfo::layout::KERNEL_BASE;

pub const PF_X: u32 = 1;
pub const PF_W: u32 = 2;
const PT_LOAD: u32 = 1;
const PHDR_SIZE: usize = 56;
const PAGE: u64 = 4096;
/// Bound against a broken or hostile header (not a count of things): the kernel image spans a few MB, and the
/// code model `kernel` cannot address more than 2 GB anyway.
pub const MAX_SPAN: u64 = 1 << 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub vaddr: u64,
    pub offset: u64,
    pub filesz: u64,
    pub memsz: u64,
    pub flags: u32,
}

#[derive(Debug)]
pub struct Elf {
    pub entry: u64,
    pub segs: Vec<Segment>,
    /// Page-aligned virtual span of all PT_LOAD segments.
    pub lo: u64,
    pub hi: u64,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

/// Checks an x86_64 ELF64 executable whose PT_LOAD segments lie in the kernel's top 2 GB, span at most
/// `MAX_SPAN` and contain the entry point in an executable segment.
pub fn parse(b: &[u8]) -> Result<Elf, &'static str> {
    if b.len() < 64 {
        return Err("too short for an ELF header (under 64 bytes)");
    }
    if b[0..4] != *b"\x7fELF" {
        return Err("not an ELF file (bad magic)");
    }
    if b[4] != 2 || b[5] != 1 {
        return Err("not a 64-bit little-endian ELF");
    }
    if u16_at(b, 18) != 0x3E {
        return Err("not built for x86_64");
    }
    if u16_at(b, 16) != 2 {
        return Err("not an executable (ELF type is not EXEC)");
    }
    let (phoff, phentsize, phnum) = (u64_at(b, 32), u16_at(b, 54) as usize, u16_at(b, 56) as usize);
    if phentsize != PHDR_SIZE {
        return Err("unexpected program header size");
    }
    let table_end = phoff.checked_add((phnum * PHDR_SIZE) as u64).ok_or("program headers out of range")?;
    if table_end > b.len() as u64 {
        return Err("program headers lie outside the file");
    }
    let mut segs = Vec::new();
    for i in 0..phnum {
        let h = &b[phoff as usize + i * PHDR_SIZE..][..PHDR_SIZE];
        if u32_at(h, 0) != PT_LOAD {
            continue;
        }
        let s = Segment { flags: u32_at(h, 4), offset: u64_at(h, 8), vaddr: u64_at(h, 16), filesz: u64_at(h, 32), memsz: u64_at(h, 40) };
        let file_end = s.offset.checked_add(s.filesz).ok_or("segment out of range")?;
        if file_end > b.len() as u64 {
            return Err("a segment lies outside the file (the file is cut short?)");
        }
        if s.filesz > s.memsz {
            return Err("a segment has more file bytes than memory bytes");
        }
        if s.vaddr < KERNEL_BASE {
            return Err("a segment lies below the kernel base 0xFFFFFFFF80000000");
        }
        if s.vaddr.checked_add(s.memsz).and_then(|e| e.checked_next_multiple_of(PAGE)).is_none() {
            return Err("a segment runs past the end of the address space");
        }
        if s.memsz > 0 {
            segs.push(s);
        }
    }
    if segs.is_empty() {
        return Err("no loadable segments");
    }
    check_layout(&mut segs)?;
    let lo = segs.iter().map(|s| s.vaddr).min().unwrap_or(0) & !(PAGE - 1);
    // Checked above: no segment end overflows when rounded up.
    let hi = segs.iter().map(|s| (s.vaddr + s.memsz).next_multiple_of(PAGE)).max().unwrap_or(0);
    if hi - lo > MAX_SPAN {
        return Err("the segments span more than 1 GiB");
    }
    let entry = u64_at(b, 24);
    let in_exec = segs.iter().any(|s| s.flags & PF_X != 0 && entry >= s.vaddr && entry < s.vaddr + s.memsz);
    if !in_exec {
        return Err("the entry point lies outside every executable segment");
    }
    Ok(Elf { entry, segs, lo, hi })
}

/// Sorts the segments by address and refuses overlapping ones, and ones with different write/execute
/// rights on a shared page: one page table entry cannot give both, and the loader would have to make text
/// writable or data executable. The kernel's linker script starts every rights change on a new page.
/// Sorted, non-overlapping segments can only share a page with their neighbour, so one pass suffices
/// (no O(n^2) on a hostile header with 65535 segments).
fn check_layout(segs: &mut [Segment]) -> Result<(), &'static str> {
    segs.sort_unstable_by_key(|s| s.vaddr);
    let rights = |s: &Segment| s.flags & (PF_W | PF_X);
    for pair in segs.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        // parse() checked that vaddr + memsz rounded up to a page does not overflow.
        let a_end = a.vaddr + a.memsz;
        if b.vaddr < a_end {
            return Err("two segments overlap");
        }
        if rights(a) != rights(b) && b.vaddr & !(PAGE - 1) < a_end.next_multiple_of(PAGE) {
            return Err("two segments with different write/execute rights share a page");
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::vec;
    use std::vec::Vec;

    /// A minimal kernel-like ELF: one RX segment at KERNEL_BASE with `code`, one RW segment with bss.
    pub fn sample(code: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 64 + 2 * PHDR_SIZE];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[6] = 1;
        b[16..18].copy_from_slice(&2u16.to_le_bytes());
        b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        b[24..32].copy_from_slice(&(KERNEL_BASE + 0x10).to_le_bytes());
        b[32..40].copy_from_slice(&64u64.to_le_bytes());
        b[54..56].copy_from_slice(&(PHDR_SIZE as u16).to_le_bytes());
        b[56..58].copy_from_slice(&2u16.to_le_bytes());
        let off = b.len() as u64;
        let ph = |b: &mut Vec<u8>, i: usize, flags: u32, offset: u64, vaddr: u64, filesz: u64, memsz: u64| {
            let h = &mut b[64 + i * PHDR_SIZE..64 + (i + 1) * PHDR_SIZE];
            h[0..4].copy_from_slice(&PT_LOAD.to_le_bytes());
            h[4..8].copy_from_slice(&flags.to_le_bytes());
            h[8..16].copy_from_slice(&offset.to_le_bytes());
            h[16..24].copy_from_slice(&vaddr.to_le_bytes());
            h[32..40].copy_from_slice(&filesz.to_le_bytes());
            h[40..48].copy_from_slice(&memsz.to_le_bytes());
        };
        ph(&mut b, 0, PF_X | 4, off, KERNEL_BASE, code.len() as u64, code.len() as u64);
        ph(&mut b, 1, PF_W | 4, off, KERNEL_BASE + 0x20_0000, 0, 0x3000);
        b.extend_from_slice(code);
        b
    }

    fn set_u64(b: &mut [u8], o: usize, v: u64) {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn good_kernel() {
        let e = parse(&sample(&[0x90; 64])).unwrap();
        assert_eq!(e.segs.len(), 2);
        assert_eq!(e.lo, KERNEL_BASE);
        assert_eq!(e.hi, KERNEL_BASE + 0x20_3000);
        assert_eq!(e.entry, KERNEL_BASE + 0x10);
    }

    #[test]
    fn bad_files_are_named() {
        let good = sample(&[0x90; 64]);
        assert_eq!(parse(b"hello").unwrap_err(), "too short for an ELF header (under 64 bytes)");
        assert_eq!(parse(&[b'x'; 200]).unwrap_err(), "not an ELF file (bad magic)");
        let mut b = good.clone();
        b[4] = 1;
        assert_eq!(parse(&b).unwrap_err(), "not a 64-bit little-endian ELF");
        let mut b = good.clone();
        b[18] = 0x28;
        assert_eq!(parse(&b).unwrap_err(), "not built for x86_64");
        let mut b = good.clone();
        b[16] = 3;
        assert!(parse(&b).unwrap_err().contains("not an executable"));
        // Cut short inside the code segment.
        assert!(parse(&good[..good.len() - 10]).unwrap_err().contains("outside the file"));
        // Program header table out of the file.
        let mut b = good.clone();
        set_u64(&mut b, 32, u64::MAX - 10);
        assert!(parse(&b).is_err());
        // A user-space address.
        let mut b = good.clone();
        set_u64(&mut b, 64 + 16, 0x40_0000);
        assert!(parse(&b).unwrap_err().contains("below the kernel base"));
        // Entry in the data segment.
        let mut b = good.clone();
        set_u64(&mut b, 24, KERNEL_BASE + 0x20_0010);
        assert!(parse(&b).unwrap_err().contains("entry point"));
        // memsz that wraps the address space.
        let mut b = good.clone();
        set_u64(&mut b, 64 + PHDR_SIZE + 40, u64::MAX - 4);
        assert!(parse(&b).unwrap_err().contains("past the end"));
        // A 1.25 GiB span.
        let mut b = good.clone();
        set_u64(&mut b, 64 + PHDR_SIZE + 40, 0x5000_0000);
        assert!(parse(&b).unwrap_err().contains("1 GiB"));
    }

    #[test]
    fn segment_layout() {
        let good = sample(&[0x90; 64]);
        let data_vaddr = 64 + PHDR_SIZE + 16;
        // Data segment moved onto the text page: different rights on one page.
        let mut b = good.clone();
        set_u64(&mut b, data_vaddr, KERNEL_BASE + 0x800);
        assert_eq!(parse(&b).unwrap_err(), "two segments with different write/execute rights share a page");
        // Data segment starting inside the text bytes.
        let mut b = good.clone();
        set_u64(&mut b, data_vaddr, KERNEL_BASE + 0x20);
        assert_eq!(parse(&b).unwrap_err(), "two segments overlap");
        // Same rights on a shared page are fine (text and a second text segment), in any header order.
        let mut b = good.clone();
        set_u64(&mut b, data_vaddr, KERNEL_BASE + 0x800);
        b[64 + PHDR_SIZE + 4..64 + PHDR_SIZE + 8].copy_from_slice(&(PF_X | 4).to_le_bytes());
        let e = parse(&b).unwrap();
        assert_eq!(e.segs[0].vaddr, KERNEL_BASE);
        // Data first in the header, text second: sorted, still accepted.
        let mut b = good.clone();
        let (h0, h1) = (64, 64 + PHDR_SIZE);
        let first: Vec<u8> = b[h0..h1].to_vec();
        b.copy_within(h1..h1 + PHDR_SIZE, h0);
        b[h1..h1 + PHDR_SIZE].copy_from_slice(&first);
        let e = parse(&b).unwrap();
        assert!(e.segs[0].vaddr < e.segs[1].vaddr);
    }

    #[test]
    fn every_truncation_is_an_error_never_a_panic() {
        let good = sample(&[0x90; 64]);
        for n in 0..good.len() {
            assert!(parse(&good[..n]).is_err(), "cut at {n}");
        }
        // Random byte flips must not panic.
        let mut x: u64 = 0x1234_5678;
        for _ in 0..20_000 {
            let mut b = good.clone();
            for _ in 0..4 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let i = (x as usize) % b.len();
                b[i] = (x >> 32) as u8;
            }
            let _ = parse(&b);
        }
    }
}
