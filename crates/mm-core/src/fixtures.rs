//! Firmware maps used by the host tests and the ktests (staged maps, V1 `k7-model.mjs` style).

use crate::map::{MapRange, RangeUse};
use alloc::vec::Vec;
use bootinfo::memtype as t;

const RESERVED: u32 = 0;

/// The Mini-PC's map (spec/core.md 8.2: 22 entries, ~24,400 MB usable, the big block 0x1_0000_0000 to
/// 0x6_5E20_0000, the 8 GB VRAM carve-out above it), with bitsboot's own types in the low area:
/// (start, end, UEFI/BitsOS memory type).
pub const MINIPC: [(u64, u64, u32); 22] = [
    (0x0, 0x9F000, t::CONVENTIONAL),
    (0x9F000, 0x10_0000, RESERVED),
    (0x10_0000, 0x100_0000, t::CONVENTIONAL),
    (0x100_0000, 0x140_0000, t::BITS_KERNEL),
    (0x140_0000, 0x148_0000, t::BITS_PAGETABLE),
    (0x148_0000, 0x149_0000, t::BITS_KEEP),
    (0x149_0000, 0x200_0000, t::LOADER_DATA),
    (0x200_0000, 0x300_0000, t::BITS_MODULE),
    (0x300_0000, 0x9A7_F000, t::CONVENTIONAL),
    (0x9A7_F000, 0xA00_0000, RESERVED),
    (0xA00_0000, 0x8C00_0000, t::CONVENTIONAL),
    (0x8C00_0000, 0x8D00_0000, t::BOOT_SERVICES_DATA),
    (0x8D00_0000, 0x9600_0000, t::BOOT_SERVICES_CODE),
    (0x9600_0000, 0x9670_0000, t::RUNTIME_SERVICES_DATA),
    (0x9670_0000, 0x9680_0000, t::ACPI_RECLAIM),
    (0x9680_0000, 0x9700_0000, t::ACPI_NVS),
    (0x9700_0000, 0x9C00_0000, RESERVED),
    (0xE000_0000, 0xF000_0000, t::MMIO),
    (0xFED8_0000, 0xFED8_1000, t::MMIO),
    (0x1_0000_0000, 0x6_5E20_0000, t::CONVENTIONAL),
    (0x6_5E20_0000, 0x6_5F34_0000, RESERVED),
    (0x6_5F34_0000, 0x8_6000_0000, RESERVED),
];

/// A typed map as ranges (the same classification as `map::from_bootinfo`).
pub fn typed(entries: &[(u64, u64, u32)]) -> Vec<MapRange> {
    entries
        .iter()
        .map(|&(s, e, ty)| {
            let usage = if t::is_free_after_handover(ty) {
                RangeUse::Free
            } else if ty == t::BITS_MODULE {
                RangeUse::Module
            } else {
                RangeUse::Reserved
            };
            MapRange::new(s, e, usage)
        })
        .collect()
}

/// V1 k7-model.mjs `pcMap(mb)`: the e820 map SeaBIOS gives an i440FX pc with `mb` MB. Returns the ranges and
/// the usable part above 1 MiB.
pub fn pc(mb: u64) -> (Vec<MapRange>, Vec<(u64, u64)>) {
    let g4 = 1u64 << 32;
    let ram = mb << 20;
    let low = if ram >= 0xE000_0000 { 0xC000_0000 } else { ram };
    let high = ram - low;
    let mut e: Vec<(u64, u64, bool)> = alloc::vec![
        (0, 0x9FC00, true),
        (0x9FC00, 0xA0000, false),
        (0xF0000, 0x10_0000, false),
        (0x10_0000, low - 0x20000, true),
        (low - 0x20000, low, false),
        (0xFFFC_0000, g4, false),
        (0xFD_0000_0000, 0x100_0000_0000, false),
    ];
    let mut usable = alloc::vec![(0x10_0000, low - 0x20000)];
    if high > 0 {
        e.push((g4, g4 + high, true));
        usable.push((g4, g4 + high));
    }
    let r = e.iter().map(|&(s, end, f)| MapRange::new(s, end, if f { RangeUse::Free } else { RangeUse::Reserved })).collect();
    (r, usable)
}
