//! Fake MTRR sets: the shapes OVMF and the Mini-PC's firmware program, and the corner cases.

use super::*;
use hal::pat::{MT_UC, MT_WB, MT_WC, MT_WP, MT_WT};

const K4: u64 = 4 << 10;
const M2: u64 = 2 << 20;
const G1: u64 = 1 << 30;

fn var(base: u64, size: u64, ty: u8) -> (u64, u64) {
    let bits = (1u64 << 48) - 1;
    (base | ty as u64, (!(size - 1) & bits & !0xFFF) | (1 << 11))
}

fn regs(def: u8, fixed_on: bool, vars: &[(u64, u64)]) -> MtrrRegs {
    let mut r = MtrrRegs {
        cap: 0x500 | vars.len() as u64,
        def_type: def as u64 | (1 << 11) | if fixed_on { 1 << 10 } else { 0 },
        var: vars.to_vec(),
        phys_bits: 48,
        ..Default::default()
    };
    // Fixed: WB everywhere except the legacy VGA hole 0xA0000-0xBFFFF (UC), as firmware sets it.
    let wb = u64::from_le_bytes([MT_WB; 8]);
    r.fixed = [wb; 11];
    r.fixed[2] = 0; // 0x259: 8 x 16 KiB from 0xA0000, all UC
    r
}

#[test]
fn ovmf_shape_pci_hole() {
    // Default WB, [2 GiB, 4 GiB) UC (the 32-bit PCI hole).
    let m = Mtrrs::from_regs(&regs(MT_WB, true, &[var(0x8000_0000, 2 * G1, MT_UC)]));
    assert_eq!(m.block_type(G1, G1), Some(MT_WB));
    assert_eq!(m.block_type(2 * G1, G1), Some(MT_UC));
    assert_eq!(m.block_type(0x7FE0_0000, M2), Some(MT_WB));
    assert_eq!(m.block_type(0x8000_0000, M2), Some(MT_UC));
    assert_eq!(m.block_type(4 * G1, G1), Some(MT_WB), "above the hole: default");
    assert_eq!(m.block_type(0, G1), None, "the first GiB holds the fixed ranges");
}

#[test]
fn fixed_ranges_below_one_mib() {
    let m = Mtrrs::from_regs(&regs(MT_WB, true, &[]));
    assert_eq!(m.block_type(0, M2), None, "a 2 MiB page at 0 crosses the fixed/variable border");
    assert_eq!(m.block_type(0x9F000, K4), Some(MT_WB));
    assert_eq!(m.block_type(0xA0000, K4), Some(MT_UC));
    assert_eq!(m.block_type(0xBF000, K4), Some(MT_UC));
    assert_eq!(m.block_type(0xC0000, K4), Some(MT_WB));
    assert_eq!(m.block_type(M2 / 2, K4), Some(MT_WB), "1 MiB: variable/default again");
    // Fixed ranges switched off: the default covers the first MiB as well.
    let mut r = regs(MT_WB, false, &[]);
    r.fixed[2] = 0;
    let m = Mtrrs::from_regs(&r);
    assert_eq!(m.block_type(0, M2), Some(MT_WB));
}

#[test]
fn boundary_inside_a_large_page_splits() {
    // WB [0, 1 GiB) with a 2 MiB UC hole at its top, and a 4 KiB-sized UC page at 0x2000_0000.
    let m = Mtrrs::from_regs(&regs(MT_UC, false, &[
        var(0, G1, MT_WB),
        var(0x3FE0_0000, M2, MT_UC),
        var(0x2000_0000, K4, MT_UC),
    ]));
    assert_eq!(m.block_type(0, G1), None);
    assert_eq!(m.block_type(0x3FE0_0000, M2), Some(MT_UC));
    assert_eq!(m.block_type(0x3FC0_0000, M2), Some(MT_WB));
    assert_eq!(m.block_type(0x2000_0000, M2), None, "the 4 KiB UC page splits its 2 MiB block");
    assert_eq!(m.block_type(0x2000_0000, K4), Some(MT_UC));
    assert_eq!(m.block_type(0x2000_1000, K4), Some(MT_WB));
    assert_eq!(m.block_type(G1, G1), Some(MT_UC), "outside every range: default");
}

#[test]
fn overlap_rules() {
    let m = Mtrrs::from_regs(&regs(MT_UC, false, &[
        var(0, G1, MT_WB),
        var(0, M2, MT_WT),
        var(M2, M2, MT_WC),
        var(2 * M2, M2, MT_WP),
        var(2 * M2, M2, MT_WP),
    ]));
    assert_eq!(m.block_type(0, M2), Some(MT_WT), "WT over WB = WT");
    assert_eq!(m.block_type(M2, M2), Some(MT_UC), "WC over WB is undefined: UC");
    assert_eq!(m.block_type(2 * M2, M2), Some(MT_UC), "WP over WB: UC");
    assert_eq!(combine(MT_WP, MT_WP), MT_WP);
    assert_eq!(combine(MT_UC, MT_WB), MT_UC);
}

#[test]
fn disabled_or_absent() {
    let mut r = regs(MT_WB, true, &[var(0, M2, MT_UC)]);
    r.def_type &= !(1 << 11);
    let m = Mtrrs::from_regs(&r);
    assert_eq!(m.block_type(0, G1), Some(MT_UC), "MTRRs off: all UC, one type");
    assert_eq!(Mtrrs::none().block_type(5 * G1, G1), Some(MT_WB));
    // A range without its valid bit is ignored.
    let mut r = regs(MT_WB, false, &[var(0, M2, MT_UC)]);
    r.var[0].1 &= !(1 << 11);
    assert_eq!(Mtrrs::from_regs(&r).block_type(0, G1), Some(MT_WB));
}

#[test]
fn non_contiguous_mask() {
    // Mask bits 30.. and bit 21 only: matches every 2 MiB block with bit 21 clear in the first GiB.
    let bits = (1u64 << 48) - 1;
    let mask = (!(G1 - 1) & bits) | (1 << 21) | (1 << 11);
    let m = Mtrrs::from_regs(&regs(MT_WB, false, &[(MT_UC as u64, mask)]));
    assert_eq!(m.block_type(0, M2), Some(MT_UC));
    assert_eq!(m.block_type(M2, M2), Some(MT_WB));
    assert_eq!(m.block_type(2 * M2, M2), Some(MT_UC));
    assert_eq!(m.block_type(0x3FE0_0000, M2), Some(MT_WB));
    assert_eq!(m.block_type(0, 2 * M2), None);
    assert_eq!(m.block_type(G1, M2), Some(MT_WB), "outside the first GiB");
    assert_eq!(m.block_type(G1, G1), Some(MT_WB));
}

#[test]
fn mini_pc_shape_with_tom2() {
    // AMD firmware: default UC, WB [0, 2 GiB) + WB [2 GiB, 2.5 GiB), RAM above 4 GiB WB only through TOM2
    // (0x6_6000_0000), the VRAM carve-out above it stays UC; the framebuffer BAR at 0x7C_0000_0000 is UC.
    let mut r = regs(MT_UC, true, &[var(0, 2 * G1, MT_WB), var(2 * G1, G1 / 2, MT_WB)]);
    r.tom2 = Some(0x6_6000_0000);
    let m = Mtrrs::from_regs(&r);
    assert_eq!(m.block_type(4 * G1, G1), Some(MT_WB));
    assert_eq!(m.block_type(0x6_4000_0000, G1), None, "TOM2 inside this GiB");
    assert_eq!(m.block_type(0x6_5E00_0000, M2), Some(MT_WB));
    assert_eq!(m.block_type(0x6_6000_0000, M2), Some(MT_UC), "carve-out: default UC");
    assert_eq!(m.block_type(0x7C_0000_0000, G1), Some(MT_UC));
    assert_eq!(m.block_type(2 * G1, G1), None, "[2.5, 3) GiB is UC");
    assert_eq!(m.block_type(0x9FE0_0000, M2), Some(MT_WB));
    assert_eq!(m.block_type(0xA000_0000, M2), Some(MT_UC));
    assert_eq!(m.block_type(3 * G1, G1), Some(MT_UC), "below 4 GiB TOM2 does not apply");
    assert_eq!(m.type_at(0x6_5FFF_F000), MT_WB);
    assert_eq!(type_name(m.type_at(0xA000_0000)), "UC");
}
