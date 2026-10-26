//! Disk images for tests (host tests and ktests build the same ones): GPT and MBR tables, a FAT boot sector.

use crate::table::{crc32, MBR_GPT};
use alloc::vec;
use alloc::vec::Vec;

fn put32(b: &mut [u8], i: usize, v: u32) {
    b[i..i + 4].copy_from_slice(&v.to_le_bytes());
}
fn put64(b: &mut [u8], i: usize, v: u64) {
    b[i..i + 8].copy_from_slice(&v.to_le_bytes());
}

/// A zeroed image of `blocks` blocks with a GPT holding `parts` (type GUID, first LBA, last LBA), 128 entries
/// of 128 bytes, primary at LBA 1, backup at the end, a protective MBR.
pub fn gpt(bs: usize, blocks: u64, parts: &[([u8; 16], u64, u64)]) -> Vec<u8> {
    let mut img = vec![0u8; bs * blocks as usize];
    let entries_blocks = (128 * 128usize).div_ceil(bs) as u64;
    // Protective MBR.
    let o = 0x1BE;
    img[o + 4] = MBR_GPT;
    put32(&mut img, o + 8, 1);
    put32(&mut img, o + 12, (blocks - 1).min(0xFFFF_FFFF) as u32);
    img[510] = 0x55;
    img[511] = 0xAA;
    let mut arr = vec![0u8; 128 * 128];
    for (i, (t, first, last)) in parts.iter().enumerate() {
        let e = &mut arr[i * 128..(i + 1) * 128];
        e[..16].copy_from_slice(t);
        e[16] = i as u8 + 1; // unique GUID
        put64(e, 32, *first);
        put64(e, 40, *last);
    }
    let arr_crc = crc32(&arr);
    let last = blocks - 1;
    let backup_entries = last - entries_blocks;
    for (my, alt, entries_lba) in [(1u64, last, 2u64), (last, 1, backup_entries)] {
        let mut h = vec![0u8; 92];
        h[..8].copy_from_slice(b"EFI PART");
        put32(&mut h, 8, 0x0001_0000);
        put32(&mut h, 12, 92);
        put64(&mut h, 24, my);
        put64(&mut h, 32, alt);
        put64(&mut h, 40, 2 + entries_blocks);
        put64(&mut h, 48, backup_entries - 1);
        put64(&mut h, 72, entries_lba);
        put32(&mut h, 80, 128);
        put32(&mut h, 84, 128);
        put32(&mut h, 88, arr_crc);
        let c = crc32(&h);
        put32(&mut h, 16, c);
        let at = my as usize * bs;
        img[at..at + 92].copy_from_slice(&h);
        let ea = entries_lba as usize * bs;
        img[ea..ea + arr.len()].copy_from_slice(&arr);
    }
    img
}

/// A zeroed image with an MBR holding `parts` (type, first LBA, blocks).
pub fn mbr(bs: usize, blocks: u64, parts: &[(u8, u32, u32)]) -> Vec<u8> {
    let mut img = vec![0u8; bs * blocks as usize];
    for (i, (t, start, len)) in parts.iter().enumerate() {
        let o = 0x1BE + 16 * i;
        img[o + 4] = *t;
        put32(&mut img, o + 8, *start);
        put32(&mut img, o + 12, *len);
    }
    img[510] = 0x55;
    img[511] = 0xAA;
    img
}

/// A FAT32 boot sector like `mkfs.fat` writes (jump, OEM, BPB, boot code bytes where an MBR has its table,
/// 55AAh); `oem` at byte 3.
pub fn boot_sector(oem: &[u8; 8], serial: u32) -> [u8; 512] {
    let mut b = [0u8; 512];
    b[0] = 0xEB;
    b[1] = 0x58;
    b[2] = 0x90;
    b[3..11].copy_from_slice(oem);
    b[11..13].copy_from_slice(&512u16.to_le_bytes());
    b[13] = 8; // sectors per cluster
    b[14] = 32; // reserved
    b[16] = 2; // FATs
    b[21] = 0xF8; // media
    b[67..71].copy_from_slice(&serial.to_le_bytes());
    b[71..82].copy_from_slice(b"BITSOS     ");
    // Boot code runs over the partition-table area: boot indicators that are not 0/80h.
    for (i, x) in b[0x5A..0x1FE].iter_mut().enumerate() {
        *x = (i as u8).wrapping_mul(37) | 1;
    }
    b[510] = 0x55;
    b[511] = 0xAA;
    b
}
