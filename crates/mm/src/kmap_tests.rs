//! Direct-map plans on the Mini-PC's map shape and fake MTRR sets; image permissions.

use super::*;
use crate::mtrr::Mtrrs;
use hal::pat::{MtrrRegs, MT_UC, MT_WB};

const M2: u64 = 2 << 20;
const G1: u64 = 1 << 30;

fn var(base: u64, size: u64, ty: u8) -> (u64, u64) {
    (base | ty as u64, (!(size - 1) & ((1u64 << 48) - 1) & !0xFFF) | (1 << 11))
}

/// Default UC, WB [0, 2.5 GiB) in two ranges, TOM2 at 0x6_6000_0000; fixed ranges with a UC VGA hole.
fn mini_pc_mtrrs() -> Mtrrs {
    let wb = u64::from_le_bytes([MT_WB; 8]);
    let mut fixed = [wb; 11];
    fixed[2] = 0;
    Mtrrs::from_regs(&MtrrRegs {
        cap: 0x508,
        def_type: MT_UC as u64 | (1 << 10) | (1 << 11),
        fixed,
        var: [var(0, 2 * G1, MT_WB), var(2 * G1, G1 / 2, MT_WB)].to_vec(),
        phys_bits: 48,
        tom2: Some(0x6_6000_0000),
    })
}

/// The measured Mini-PC map (spec/core.md 8.2), simplified to its RAM ranges; the third range reaches past
/// 2.5 GiB into the default-UC part on purpose (RAM of two types).
fn mini_pc_ram() -> Vec<(u64, u64)> {
    page_ranges([(0x1000, 0x9F000), (0x10_0000, 0x9A7_F000), (0x9B0_0000, 0xB000_0000), (0x1_0000_0000, 0x6_5E20_0000)])
}

fn plan(ram: &[(u64, u64)], ro: &[(u64, u64)], m: &Mtrrs, gb: bool) -> Vec<Chunk> {
    let mut v = Vec::new();
    plan_direct_map(ram, ro, m, gb, &mut |c| v.push(c));
    v
}

/// Every RAM page exactly once, in order, nothing outside RAM, each chunk of one MTRR type page by page.
fn check_cover(chunks: &[Chunk], ram: &[(u64, u64)], m: &Mtrrs) {
    let mut it = chunks.iter().peekable();
    for &(s, e) in ram {
        let mut a = s;
        while a < e {
            let c = it.next().unwrap_or_else(|| panic!("RAM at {a:#x} not mapped"));
            assert_eq!(c.phys, a, "gap or overlap at {a:#x}");
            let n = c.size.bytes();
            assert!(c.phys.is_multiple_of(n) && c.phys + n <= e, "chunk {c:?} not aligned or past RAM");
            let mut p = c.phys;
            while p < c.phys + n {
                assert_eq!(m.type_at(p), c.mtrr, "chunk {c:?} crosses an MTRR boundary at {p:#x}");
                p += if c.size == hal::paging::PageSize::Size4K { 0x1000 } else { M2.min(n) / 512 };
            }
            a += n;
        }
    }
    assert!(it.next().is_none(), "chunks beyond RAM");
}

#[test]
fn mini_pc_plan_uses_large_pages_and_never_crosses_types() {
    let (ram, m) = (mini_pc_ram(), mini_pc_mtrrs());
    let chunks = plan(&ram, &[], &m, true);
    check_cover(&chunks, &ram, &m);
    let count = |s| chunks.iter().filter(|c| c.size == s).count();
    let (g, mb2, k4) = (count(PageSize::Size1G), count(PageSize::Size2M), count(PageSize::Size4K));
    assert_eq!(g, 1 + 21, "[1, 2) GiB and [4, 25) GiB");
    assert!(mb2 > 0 && k4 > 0);
    assert!(k4 < 2048, "4 KiB pages only at the edges: {k4}");
    assert!(chunks.iter().all(|c| c.writable));
    // The first MiB (fixed ranges) is 4 KiB pages; the VGA hole is not RAM here.
    assert!(chunks.iter().filter(|c| c.phys < 0x10_0000).all(|c| c.size == PageSize::Size4K));
    // [2 GiB, 2.5 GiB) is WB, [2.5 GiB, 0xB000_0000) default UC: split at 2.5 GiB.
    let at = |p: u64| chunks.iter().find(|c| c.phys <= p && p < c.phys + c.size.bytes()).copied().unwrap();
    assert_eq!((at(0x9FFF_F000).mtrr, at(0xA000_0000).mtrr), (MT_WB, MT_UC));
    assert_eq!(at(0x9FFF_F000).size, PageSize::Size2M);
    assert_eq!(at(0x6_5E1F_F000).mtrr, MT_WB, "high RAM up to its end is WB through TOM2");
}

#[test]
fn without_gb_pages_only_2m_and_4k() {
    let (ram, m) = (mini_pc_ram(), mini_pc_mtrrs());
    let chunks = plan(&ram, &[], &m, false);
    check_cover(&chunks, &ram, &m);
    assert!(chunks.iter().all(|c| c.size != PageSize::Size1G));
}

#[test]
fn read_only_ranges_split_and_stay_read_only() {
    let m = Mtrrs::none();
    let ram = page_ranges([(0, 4 * G1)]);
    // The kernel's text and rodata at 0x20_3000..0x2C_0000 (not 2 MiB aligned at either end).
    let ro = page_ranges([(0x20_3000, 0x2C_0000)]);
    let chunks = plan(&ram, &ro, &m, true);
    check_cover(&chunks, &ram, &m);
    for c in &chunks {
        let inside = c.phys >= 0x20_3000 && c.phys < 0x2C_0000;
        assert_eq!(c.writable, !inside, "{c:?}");
    }
    assert!(chunks.iter().any(|c| c.size == PageSize::Size1G && c.phys == G1));
    assert_eq!(chunks.iter().filter(|c| !c.writable).count(), (0x2C_0000 - 0x20_3000) / 0x1000);
    let f = direct_flags(&chunks[0]);
    assert!(f.contains(PageFlags::GLOBAL | PageFlags::NX | PageFlags::WRITABLE));
}

#[test]
fn ranges_are_cut_inward_and_merged() {
    assert_eq!(page_ranges([(0x1800, 0x5800), (0x5000, 0x9000), (0x20_0000, 0x20_0800), (u64::MAX - 5, u64::MAX)]),
        [(0x2000, 0x9000)]);
}

#[test]
fn image_permissions() {
    assert_eq!(ImagePerm::from_loader(false, true), ImagePerm::Text);
    assert_eq!(ImagePerm::from_loader(true, true), ImagePerm::WriteExec);
    let t = ImagePerm::Text.flags();
    assert!(!t.contains(PageFlags::WRITABLE) && !t.contains(PageFlags::NX) && t.contains(PageFlags::GLOBAL));
    assert!(ImagePerm::ReadOnly.flags().contains(PageFlags::NX));
    assert!(ImagePerm::Data.flags().contains(PageFlags::NX | PageFlags::WRITABLE));
    let k = 0xFFFF_FFFF_8000_0000u64;
    let s = KernelSections { text: (k, k + 0x82000), rodata: (k + 0x82000, k + 0xB8018), data: (k + 0xB9000, k + 0xE9000) };
    assert_eq!(s.perm_of(k + 0x81000), Some(ImagePerm::Text));
    assert_eq!(s.perm_of(k + 0xB8000), Some(ImagePerm::ReadOnly));
    assert_eq!(s.perm_of(k + 0xE8FFF), Some(ImagePerm::Data));
    assert_eq!(s.perm_of(k + 0xF0000), None);
    assert_eq!(image_perm(Some(&s), k, ImagePerm::Text), (ImagePerm::Text, false));
    assert_eq!(image_perm(Some(&s), k, ImagePerm::Data), (ImagePerm::Data, true), "the loader's W^X page stays, the mismatch is reported");
    assert_eq!(image_perm(Some(&s), k + 0xB8000, ImagePerm::Text), (ImagePerm::Text, true), "rodata in an RX segment stays executable");
    assert_eq!(image_perm(Some(&s), k + 0xB8000, ImagePerm::WriteExec), (ImagePerm::ReadOnly, true), "the sections split a W+X page");
    assert_eq!(image_perm(Some(&s), k + 0xF0000, ImagePerm::WriteExec), (ImagePerm::WriteExec, false), "outside the sections");
    assert_eq!(image_perm(None, k, ImagePerm::ReadOnly), (ImagePerm::ReadOnly, false));
}

#[test]
fn clamp_to_the_direct_map() {
    let r = [(0x1000, 0x9F000), (0x10_0000, 0x8000_0000), (0x1_0000_0000, 0x3_0000_0000), (0x4_0000_0000, 0x5_0000_0000)];
    let (kept, dropped) = clamp_ranges(&r, 0x2_0000_0000);
    assert_eq!(kept, [(0x1000, 0x9F000), (0x10_0000, 0x8000_0000), (0x1_0000_0000, 0x2_0000_0000)]);
    assert_eq!(dropped, 0x1_0000_0000 + 0x1_0000_0000);
    let (kept, dropped) = clamp_ranges(&r, u64::MAX);
    assert_eq!((kept.as_slice(), dropped), (&r[..], 0));
    assert_eq!(clamp_ranges(&r, 0), (Vec::new(), r.iter().map(|x| x.1 - x.0).sum()));
}
