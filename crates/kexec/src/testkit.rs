//! Synthetic kernels for tests (host tests and ktests): small ELF64 files built from segment descriptions.

use alloc::vec;
use alloc::vec::Vec;

pub struct Seg<'a> {
    pub vaddr: u64,
    pub flags: u32,
    pub data: &'a [u8],
    pub memsz: u64,
}

/// An x86_64 EXEC ELF with the given PT_LOAD segments (data at page-aligned file offsets from 0x1000) and entry.
pub fn elf(segs: &[Seg<'_>], entry: u64) -> Vec<u8> {
    let mut f = vec![0u8; 0x1000];
    f[0..4].copy_from_slice(b"\x7fELF");
    f[4] = 2;
    f[5] = 1;
    f[6] = 1;
    f[16..18].copy_from_slice(&2u16.to_le_bytes());
    f[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
    f[20..24].copy_from_slice(&1u32.to_le_bytes());
    f[24..32].copy_from_slice(&entry.to_le_bytes());
    f[32..40].copy_from_slice(&64u64.to_le_bytes());
    f[52..54].copy_from_slice(&64u16.to_le_bytes());
    f[54..56].copy_from_slice(&56u16.to_le_bytes());
    f[56..58].copy_from_slice(&(segs.len() as u16).to_le_bytes());
    let mut off = 0x1000u64;
    let mut body = Vec::new();
    for (i, s) in segs.iter().enumerate() {
        let h = 64 + i * 56;
        f[h..h + 4].copy_from_slice(&1u32.to_le_bytes());
        f[h + 4..h + 8].copy_from_slice(&s.flags.to_le_bytes());
        f[h + 8..h + 16].copy_from_slice(&off.to_le_bytes());
        f[h + 16..h + 24].copy_from_slice(&s.vaddr.to_le_bytes());
        f[h + 24..h + 32].copy_from_slice(&s.vaddr.to_le_bytes());
        f[h + 32..h + 40].copy_from_slice(&(s.data.len() as u64).to_le_bytes());
        f[h + 40..h + 48].copy_from_slice(&s.memsz.to_le_bytes());
        f[h + 48..h + 56].copy_from_slice(&0x1000u64.to_le_bytes());
        body.extend_from_slice(s.data);
        let pad = (s.data.len() as u64).next_multiple_of(0x1000) - s.data.len() as u64;
        body.extend(core::iter::repeat_n(0u8, pad as usize));
        off += s.data.len() as u64 + pad;
    }
    f.extend_from_slice(&body);
    f
}

/// A small kernel-like ELF: text (R X) with a recognisable pattern, rodata (R), data (RW) and bss.
pub fn small_kernel() -> Vec<u8> {
    let text: Vec<u8> = (0..5000u32).map(|i| (i * 7 + 1) as u8).collect();
    let rodata: Vec<u8> = (0..300u32).map(|i| (i * 3) as u8).collect();
    let data: Vec<u8> = (0..100u32).map(|i| 0xA0 ^ i as u8).collect();
    let base = bootinfo::layout::KERNEL_BASE;
    elf(
        &[
            Seg { vaddr: base, flags: 5, data: &text, memsz: text.len() as u64 },
            Seg { vaddr: base + 0x2000, flags: 4, data: &rodata, memsz: rodata.len() as u64 },
            Seg { vaddr: base + 0x2200, flags: 6, data: &data, memsz: 0x3000 },
        ],
        base + 0x10,
    )
}
