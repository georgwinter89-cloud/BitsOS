//! Piece 13: the RLC's clear-state buffer for GFX 12, as Linux builds it from `gfx12_cs_data`
//! (clearstate_gfx12.h:106-119): one section of context registers in six extents, every value 0x00000000 (all
//! 62 of the header's values are zero, so the table carries only each extent's first register and its count).
//! `gfx_v12_0_get_csb_size` (gfx_v12_0.c:677-694) counts 1 + per extent (2 + its registers), and
//! `gfx_v12_0_get_csb_buffer` (:696-722) writes the extents' count, then per extent its count, its first
//! register and its values. Linux allocates `size * 4` bytes on a page in VRAM or GTT (`amdgpu_gfx_rlc_init_csb`,
//! amdgpu_rlc.c:128-148; VRAM on this card: MC 0x83_daafc000) and tells the RLC its address with the low two
//! bits cleared and its size in dwords (`gfx_v12_0_init_csb` :1938-1948).

use alloc::vec::Vec;

/// (first register, number of registers) per extent of `gfx12_SECT_CONTEXT_defs`.
pub const EXTENTS: [(u32, u32); 6] = [(0xA03E, 34), (0xA0CC, 2), (0xA0D8, 1), (0xA0DB, 6), (0xA2E5, 11), (0xA3C0, 8)];

/// `gfx_v12_0_get_csb_size`: 75 (0x4b) dwords.
pub const fn size_dw() -> u32 {
    let mut n = 1;
    let mut i = 0;
    while i < EXTENTS.len() {
        n += 2 + EXTENTS[i].1;
        i += 1;
    }
    n
}

pub const BYTES: u64 = size_dw() as u64 * 4;

/// `gfx_v12_0_get_csb_buffer`: the buffer's dwords.
pub fn buffer() -> Vec<u32> {
    let mut b = Vec::with_capacity(size_dw() as usize);
    b.push(EXTENTS.len() as u32);
    for (reg, count) in EXTENTS {
        b.push(count);
        b.push(reg);
        b.resize(b.len() + count as usize, 0);
    }
    b
}

/// The buffer as bytes, little-endian (what lands in VRAM).
pub fn bytes() -> Vec<u8> {
    buffer().into_iter().flat_map(u32::to_le_bytes).collect()
}

/// `clear_state_gpu_addr & 0xfffffffc` and `>> 32`: (ADDR_LO, ADDR_HI).
pub const fn address_regs(mc: u64) -> (u32, u32) {
    ((mc as u32) & 0xFFFF_FFFC, (mc >> 32) as u32)
}
