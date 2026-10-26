//! Staged firmware maps, as V1's `k7-model.mjs` stages them: SeaBIOS e820 shapes of several sizes, the
//! Mini-PC's measured UEFI map (spec/core.md 8.2) through a real BootInfo record, and random maps with holes,
//! overlaps and unaligned entries checked against a per-KiB oracle.
use super::*;
use crate::map::{self, LOW_RESERVED};
use bootinfo::{memtype as t, MemDesc};

fn bytes(blocks: &[(u64, u32)]) -> u64 {
    blocks.iter().map(|&(_, o)| PAGE << o).sum()
}

/// Takes everything (NORMAL first, then DMA32), checks it against `allowed`, frees it and checks again.
fn exercise(b: &BuddyFrames, allowed: &[(u64, u64)]) -> Vec<(u64, u32)> {
    let free_before = b.stats().free_bytes;
    let mut all = take_all(b, Zone::Normal);
    all.extend(take_all(b, Zone::Dma32));
    assert_eq!(bytes(&all), free_before, "everything free was handed out exactly once");
    let m = b.meta_range();
    let ok: Vec<(u64, u64)> = map::subtract(allowed, &[m]);
    assert_disjoint_inside(&all, &ok);
    for &(p, o) in &all {
        b.free(p, o, Kind::Other);
    }
    b.check().unwrap();
    assert_eq!(b.stats().free_bytes, free_before);
    all
}

#[test]
fn seabios_maps_of_several_sizes() {
    for mb in [64, 512, 2048, 3584, 4096, 6144] {
        let (r, usable) = crate::fixtures::pc(mb);
        let b = BuddyFrames::staged("pc", &r, Storage::Carve).unwrap();
        let usable_bytes: u64 = usable.iter().map(|u| u.1 - u.0).sum();
        assert_eq!(b.stats().total_bytes, usable_bytes, "{mb} MB: total = usable above 1 MiB");
        let (ms, me) = b.meta_range();
        assert!(me > ms && usable.iter().any(|u| u.0 <= ms && me <= u.1), "{mb} MB: tables carved from free memory");
        assert_eq!(b.stats().used_by_kind[Kind::Other as usize], me - ms);
        b.free(ms, 0, Kind::Other);
        assert_eq!(b.reports().last_bad_free(), Some((ms, crate::BadFree::OutOfRange)), "the tables are not a block");
        assert_eq!(b.zone_info(1).managed_frames > 0, mb > 3072, "{mb} MB: NORMAL zone only with RAM above 4 GiB");
        exercise(&b, &usable);
    }
}

fn minipc_descs() -> Vec<MemDesc> {
    crate::fixtures::MINIPC.iter().map(|&(s, end, ty)| MemDesc { ty, phys: s, virt: 0, pages: (end - s) / PAGE, attr: 0xF }).collect()
}

#[test]
fn minipc_map_through_bootinfo() {
    let descs = minipc_descs();
    let mut buf = std::vec![0u8; 4096];
    let mut w = bootinfo::Writer::new(&mut buf).unwrap();
    w.memory_map(descs.iter().copied()).unwrap();
    let bi = bootinfo::BootInfo::parse(&buf).unwrap();
    let mm = bi.memory_map().unwrap();
    assert_eq!(mm.len(), 22);
    let seed = (0x1_0000_0000, 0x1_0080_0000); // the kernel's 8 MiB heap seed at the bottom of the big block
    let mut r = map::from_bootinfo(&mm);
    r.push(rsv(seed.0, seed.1));
    let b = BuddyFrames::staged("minipc", &r, Storage::Carve).unwrap();

    let s = b.stats();
    let mb = s.total_bytes >> 20;
    assert!((24_300..=24_450).contains(&mb), "hw_info memory: about 24,400 MB usable, got {mb} MB");
    assert_eq!(s.free_bytes + s.used_by_kind.iter().sum::<u64>(), s.total_bytes);
    let (ms, me) = b.meta_range();
    assert!(me - ms < s.total_bytes / 1000, "tables below 0.1 % of RAM: {} KiB", (me - ms) >> 10);
    assert!(ms >= 0x1_0000_0000 && me <= 0x6_5E20_0000, "carved from the big block");
    let dma32 = (b.zone_info(0).managed_frames * PAGE) >> 20;
    assert!((2_300..=2_400).contains(&dma32), "DMA32 ~2.3 GB, got {dma32} MB");
    assert!(b.region_count() <= 8, "{} regions", b.region_count());

    // Free = conventional + loader + boot services above 1 MiB, minus the seed; never KEEP/KERNEL/
    // PAGETABLE/MODULE/ACPI/runtime/VRAM.
    let usable: Vec<(u64, u64)> = descs
        .iter()
        .filter(|d| t::is_free_after_handover(d.ty) && d.end() > LOW_RESERVED)
        .map(|d| (d.phys.max(LOW_RESERVED), d.end()))
        .collect();
    let usable = map::subtract(&map::normalize(usable), &[seed]);
    let all = exercise(&b, &usable);
    for d in descs.iter().filter(|d| !t::is_free_after_handover(d.ty)) {
        assert!(all.iter().all(|&(p, o)| p + (PAGE << o) <= d.phys || p >= d.end()), "type {:#x} handed out", d.ty);
    }
    assert!(all.iter().any(|&(p, o)| o == crate::MAX_ORDER && p >= 4 * GIB), "1 GiB blocks in the big block");
}

/// Random maps on a 64 MiB address space with entries on 1 KiB boundaries: overlapping, unsorted, holes.
#[test]
fn random_maps_match_the_oracle() {
    const KIB: u64 = 1024;
    const SPACE: u64 = 64 * MIB;
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for round in 0..60 {
        let mut r = Vec::new();
        for _ in 0..1 + rng.below(30) {
            let s = rng.below(SPACE / KIB) * KIB;
            let span = if rng.below(4) == 0 { 16 * 1024 } else { 64 };
            let len = (1 + rng.below(span)) * KIB;
            let usage = match rng.below(6) {
                0 => RangeUse::Reserved,
                1 => RangeUse::Module,
                _ => RangeUse::Free,
            };
            r.push(MapRange::new(s, (s + len).min(SPACE), usage));
        }
        // Oracle: a page is free iff the free entries cover all of it and no other entry touches it.
        let n = (SPACE / KIB) as usize;
        let (mut fr, mut other) = (std::vec![false; n], std::vec![false; n]);
        for m in &r {
            for k in m.start / KIB..m.end / KIB {
                if m.usage == RangeUse::Free { fr[k as usize] = true } else { other[k as usize] = true }
            }
        }
        let page_free = |p: u64| p * PAGE >= LOW_RESERVED && (p * 4..p * 4 + 4).all(|k| fr[k as usize] && !other[k as usize]);
        let pages: Vec<u64> = (0..SPACE / PAGE).filter(|&p| page_free(p)).collect();
        let allowed = map::normalize(pages.iter().map(|&p| (p * PAGE, (p + 1) * PAGE)).collect());

        let b = staged(&r);
        assert_eq!(b.stats().total_bytes, pages.len() as u64 * PAGE, "round {round}");
        exercise(&b, &allowed);
        if let Ok(c) = BuddyFrames::staged("carve", &r, Storage::Carve) {
            assert_eq!(c.stats().total_bytes, b.stats().total_bytes, "round {round}: carving only moves frames to Other");
            exercise(&c, &allowed);
        }
    }
}

#[test]
fn empty_and_hostile_maps_do_not_panic() {
    let b = staged(&[]);
    assert_eq!(b.alloc(0, Zone::Normal, Kind::Other), None);
    b.check().unwrap();
    let b = staged(&[free(u64::MAX - 3 * PAGE, u64::MAX), free(5, 3), rsv(0, u64::MAX / 2)]);
    assert_eq!(b.stats().total_bytes, 2 * PAGE, "the last whole pages below 2^64");
    let p = b.alloc(0, Zone::Normal, Kind::Other).unwrap();
    b.free(p, 0, Kind::Other);
    b.free(!(PAGE - 1), 0, Kind::Other);
    b.check().unwrap();
}
