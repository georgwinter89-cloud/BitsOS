//! Wave 42: the card's VRAM manager (`vram::Bump`), the GART (`gart12`) and the comparison's teeth for this wave's
//! stages. Nothing handed out overlaps a kept range or another handed-out one; a region learned late is refused
//! when something already lies in it; a page is mapped and unmapped with Linux's flush, and never above 44 bits.

use super::rdna4fix::*;
use super::rdna4nb::{regs, up};
use crate::gpucore::bringup::{Bringup, Gate};
use crate::gpucore::psp::BoAlloc;
use crate::gpucore::trail::Access;
use crate::rdna4::attach::BarFact;
use crate::rdna4::fakenb::SYS_BASE;
use crate::rdna4::gart12::{self, FlushRegs, GartStop};
use crate::rdna4::guard::Mc;
use crate::rdna4::host::Host;
use crate::rdna4::linuxseq::compare::Compare;
use crate::rdna4::linuxseq::fold::{Bus, fold};
use crate::rdna4::linuxseq::{self, What};
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::nbseq;
use crate::rdna4::stage::{Session, Stage};
use crate::rdna4::vram::{self, Bump, GUARD, Kept, Layout, RangeStop, WINDOW};
use crate::attach::Gop;
use rdna4_core::pte12::{self, PteError};
use std::vec::Vec;

const VRAM: u64 = 16304 << 20;

fn layout() -> Layout {
    let bar0 = BarFact { phys: 0xA0_0000_0000, bytes: 16 << 30, is64: true, prefetch: true };
    vram::place(VRAM, 0x80_0000_0000, bar0, Some(Gop { phys: bar0.phys, bytes: PICTURE })).unwrap()
}

fn apart(b: &Bump) {
    let mut all: Vec<(u64, u64)> = b.kept().iter().map(|(_, r)| (r.start, r.end)).chain(b.taken.iter().map(|(_, r)| (r.start, r.end))).collect();
    all.sort_unstable();
    assert!(all.windows(2).all(|w| w[0].1 <= w[1].0), "{all:x?}");
    let window = b.kept().iter().find(|(k, _)| *k == Kept::PspWindow).unwrap().1.clone();
    assert!(b.given.iter().all(|g| g.start >= window.start && g.end <= window.end));
    assert!(b.given.windows(2).all(|w| w[0].end <= w[1].start));
}

#[test]
fn nothing_handed_out_overlaps_what_is_kept() {
    let l = layout();
    let mut b = Bump::new(&l);
    // The picture with its guard and the secure processor's window are kept from the first moment.
    assert_eq!(b.kept(), [(Kept::Picture, 0..l.window.start), (Kept::PspWindow, l.window.clone())]);
    assert_eq!((l.window.start, l.window.end), (PICTURE.next_multiple_of(GUARD) + GUARD, PICTURE.next_multiple_of(GUARD) + GUARD + WINDOW));
    // Free space begins behind the window; sizes round up to pages, alignment is honoured.
    let table = b.take("table", 1 << 20, 4096).unwrap();
    assert_eq!(table, l.window.end..l.window.end + (1 << 20));
    let small = b.take("small", 100, 4096).unwrap();
    assert_eq!(small, table.end..table.end + 4096);
    let wide = b.take("wide", 3 << 20, 2 << 20).unwrap();
    assert!(wide.start.is_multiple_of(2 << 20) && wide.start >= small.end);
    // The hole the alignment left is used by the next that fits.
    let hole = b.take("hole", 8192, 4096).unwrap();
    assert_eq!(hole.start, small.end);
    // The secure processor's stages get parts of the window, upwards, and nothing else does.
    let (_, ring) = b.alloc(4096, 4096).unwrap();
    let (_, pri) = b.alloc(1 << 20, 1 << 20).unwrap();
    assert!(ring.start == l.window.start && pri.start.is_multiple_of(1 << 20) && b.holds(pri.start, 1 << 20) && !b.holds(table.start, 4096));
    apart(&b);
    // Giving back: the same range is handed out again; only a whole taken range goes back.
    assert!(b.give_back(&small) && !b.give_back(&small) && !b.give_back(&(table.start..table.start + 4096)));
    assert_eq!(b.take("again", 4096, 4096).unwrap(), small);
    // The window comes back whole only; its parts are not reused before.
    let (_, next) = b.alloc(4096, 4096).unwrap();
    assert!(next.start >= pri.end);
    b.window_back();
    assert_eq!(b.alloc(4096, 4096).unwrap().1, ring);
    apart(&b);
    assert_eq!(b.take("zero", 0, 4096), Err(RangeStop::Bad { what: "zero" }));
    assert_eq!(b.take("odd", 4096, 3000), Err(RangeStop::Bad { what: "odd" }));
}

#[test]
fn the_firmwares_region_is_unknown_at_first_and_kept_once_known() {
    let l = layout();
    let mut b = Bump::new(&l);
    // Before the secure OS named it nothing lies above half of VRAM, where Linux saw it.
    let big = b.take("big", VRAM / 2 - l.window.end - (1 << 20), 4096).unwrap();
    assert!(big.end <= VRAM / 2);
    assert_eq!(b.take("more", 2 << 20, 4096), Err(RangeStop::NoRoom { what: "more", bytes: 2 << 20 }));
    // A region that lies over something handed out, over the window or over the picture is refused whole.
    for bad in [big.start + 4096..big.start + 8192, l.window.start..l.window.start + 4096, 0..4096, VRAM - 4096..VRAM + 4096, 5..5] {
        assert!(b.keep_firmware(core::slice::from_ref(&bad)).is_err(), "{bad:x?}");
    }
    assert!(matches!(b.keep_firmware(&[big.start..VRAM]), Err(RangeStop::Overlap { over: "big", .. })));
    // Two regions are kept together or not at all: a free first one is not kept when the second lies over
    // something, and the ceiling stays at half of VRAM.
    let free = VRAM - (50 << 20)..VRAM;
    assert!(matches!(b.keep_firmware(&[free.clone(), l.window.start..l.window.start + 4096]), Err(RangeStop::Overlap { over: "a kept range", .. })));
    assert_eq!(b.kept().len(), 2, "a refused region is not kept, nor the one named with it");
    assert_eq!(b.take("more", 2 << 20, 4096), Err(RangeStop::NoRoom { what: "more", bytes: 2 << 20 }));
    // The region as Linux saw it: the top 516 MiB. From then on the rest of VRAM is free, the region never.
    let top = VRAM - (516 << 20)..VRAM;
    b.keep_firmware(core::slice::from_ref(&top)).unwrap();
    b.keep_firmware(&[top.clone(), top.clone()]).unwrap();
    assert_eq!(b.kept().len(), 3, "the same region twice is one region");
    let high = b.take("high", 1 << 30, 4096).unwrap();
    assert!(high.start >= big.end && high.end > VRAM / 2 && high.end <= top.start);
    assert!(matches!(b.take("too much", 15 << 30, 4096), Err(RangeStop::NoRoom { .. })));
    apart(&b);
}

#[test]
fn a_region_over_the_gart_table_ends_the_ring_stage() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (_, m) = up(&rig, &mut s, model());
    let (_, mut m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Sos]);
    let table = s.nb.gart.as_ref().unwrap().table.clone();
    // A secure OS that claims the memory the GART table was put in before it could say so.
    m.reserved_at = Some(table.start);
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("gehört der Karte selbst"), "{}", d.line);
    assert!(m.ring.is_none(), "the ring is destroyed again");
    assert!(!s.bump.as_ref().unwrap().kept().iter().any(|(k, _)| *k == Kept::Firmware));
}

/// The GART of a card after `nbif` and `gmc`, behind an owner that keeps the trail, with MC addresses.
fn gart_rig() -> (Rig, Session, Bringup<Mc<crate::rdna4::fakemodel::Model>>, FlushRegs) {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut m = model();
    for st in [Stage::Nbif, Stage::Gmc] {
        let (d, back) = rig.run(&mut s, st, m, &mut Stick::full());
        assert!(d.pass);
        m = back;
    }
    let n = NbRegs::from(&rig.att.discovery, &regs(&rig)).unwrap();
    let f = FlushRegs::of(&n, s.nb.hdp_flush(n.hdp_flush)).unwrap();
    let l = s.layout.clone().unwrap();
    let owner = Bringup::new(Mc { hw: m, base: l.mc_base, bytes: l.vram_bytes }, Gate::Allow, 64).with_trail(256).without_read_back();
    (rig, s, owner, f)
}

#[test]
fn a_page_is_mapped_and_unmapped_with_linuxs_flush() {
    let (rig, mut s, mut hw, f) = gart_rig();
    let gart = s.nb.gart.as_mut().unwrap();
    let pages: Vec<u64> = (0..3).map(|i| SYS_BASE + i * 4096).collect();
    let mut out = Vec::new();
    let mc = gart.map(&mut hw, &f, "three pages", &pages, &mut out).unwrap();
    assert_eq!(mc, 0x1000, "the lowest free place; page 0 stays unbound");
    assert_eq!((gart.system_of(mc + 0x2004), gart.system_of(0), gart.system_of(mc + 0x3000)), (Some(SYS_BASE + 0x2004), None, None));
    // The flush after the bind is Linux's nine lines, held line by line against the trace's first one.
    let trail: Vec<Access> = hw.trail().unwrap().recent().copied().collect();
    let did = fold(&trail, &Bus::of(&regs(&rig))).unwrap();
    let unit = linuxseq::bind_flush().unwrap();
    assert_eq!(did.len(), unit.len());
    let sw = linuxseq::section("sw-init").unwrap();
    let mut c = Compare::new(&sw, &[]).unwrap();
    c.feed(&did).unwrap_or_else(|d| panic!("{d}"));
    assert_eq!(c.stop().unwrap(), Some(221), "the next line of sw-init is Linux's second flush");
    assert!(matches!(unit[0].what, What::Hdp { flush: 0x1fc00, .. }));
    // A second buffer goes behind the first; unmapping the first frees its place for the next.
    let two = gart.map(&mut hw, &f, "two pages", &[SYS_BASE + (1 << 20), SYS_BASE + (2 << 20)], &mut out).unwrap();
    assert_eq!(two, 0x4000);
    gart.unmap(&mut hw, &f, mc, &mut out).unwrap();
    assert_eq!(gart.system_of(mc), None);
    assert_eq!(gart.unmap(&mut hw, &f, mc, &mut out), Err(GartStop::NotBound { mc }));
    assert_eq!(gart.map(&mut hw, &f, "again", &pages[..2], &mut out).unwrap(), 0x1000);
    assert!(out.is_empty(), "no wait ran out: {out:?}");
    let table = gart.table.start;
    let dummy = gart.dummy;
    let m = hw.into_hw().hw;
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    assert_eq!(m.nb.tlb_flushes, 1 + 4, "gmc's, then one per map and unmap");
    // The table: bound entries are GFX 12's, the unmapped one is the dummy page with no flag.
    let mut e = [0u8; 8 * 5];
    m.vram_peek(table + 8, &mut e);
    let entry = |i: usize| u64::from_le_bytes(e[i * 8..i * 8 + 8].try_into().unwrap());
    assert_eq!([entry(0), entry(1), entry(2), entry(3)], [pte12::bound(pages[0]).unwrap(), pte12::bound(pages[1]).unwrap(), pte12::unbound(dummy), pte12::bound(SYS_BASE + (1 << 20)).unwrap()]);
}

#[test]
fn a_page_above_44_bits_is_never_mapped() {
    let (_, mut s, mut hw, f) = gart_rig();
    let gart = s.nb.gart.as_mut().unwrap();
    let mut out = Vec::new();
    for (pages, why) in [(std::vec![SYS_BASE, 1 << 44], PteError::PastCeiling(1 << 44)), (std::vec![SYS_BASE + 1], PteError::Unaligned(SYS_BASE + 1))] {
        assert_eq!(gart.map(&mut hw, &f, "bad", &pages, &mut out), Err(GartStop::Page(why)));
    }
    assert_eq!(gart.map(&mut hw, &f, "none", &[], &mut out), Err(GartStop::Full { pages: 0 }));
    let too_many: Vec<u64> = (0..gart.pages()).map(|i| SYS_BASE + i * 4096).collect();
    assert_eq!(gart.map(&mut hw, &f, "all", &too_many, &mut out), Err(GartStop::Full { pages: too_many.len() }));
    assert!(gart.bound.is_empty());
    // Nothing reached the card: no entry, no flush.
    assert_eq!(hw.trail().unwrap().recent().count(), 0);
    assert_eq!(hw.log().totals().writes, 0);
    assert!(hw.hw().hw.pages(4096).is_some());
    // The HDP flush alone, as Linux does it: 0 to the remapped place, then the memory size.
    gart12::flush_hdp(&mut hw, &f).unwrap();
    let t: Vec<Access> = hw.trail().unwrap().recent().copied().collect();
    assert!(matches!(t[..], [Access::Write { byte: 0x7f000, value: 0, reached: true }, Access::Read { value: Some(0x3fb0), .. }]), "{t:?}");
}

#[test]
fn the_comparison_names_a_stage_that_is_not_linuxs() {
    let rig = Rig::new();
    let r = regs(&rig);
    let run = |change: &dyn Fn(&mut crate::rdna4::fakemodel::Model)| {
        let mut s = Session::default();
        let mut m = model();
        change(&mut m);
        let mut out = Vec::new();
        for st in NB {
            let (d, back) = rig.run(&mut s, st, m, &mut Stick::full());
            m = back;
            out.push(d);
        }
        out
    };
    // A card that answers a read differently from the real one: named at Linux's line, with both values.
    let done = run(&|m| m.nb.strap2 = 0x0313_2080);
    let e = nbseq::equal(&done[0], &r, VRAM).unwrap_err();
    assert!(e.starts_with("nbif.txt line 2 (init.txt line 9): Linux reads RCC_DEV0_EPF2_STRAP2 (the card answered 0x03132000); the stage reads RCC_DEV0_EPF2_STRAP2 (its card answered 0x03132080)"), "{e}");
    // The stage's own trail with one access left out, two swapped, one value changed, one added.
    let done = run(&|_| {});
    for good in &done {
        // Two writes in a row, somewhere past the stage's first accesses.
        let at = (6..good.trail.len() - 1).find(|i| matches!((good.trail[*i], good.trail[*i + 1]), (Access::Write { byte: a, .. }, Access::Write { byte: b, .. }) if a != b)).unwrap();
        let mut gone = good.clone();
        gone.trail.remove(at);
        let mut swapped = good.clone();
        swapped.trail.swap(at, at + 1);
        let mut other = good.clone();
        if let Access::Write { value, .. } = &mut other.trail[at] {
            *value ^= 0x8000;
        }
        let mut extra = good.clone();
        extra.trail.insert(at, Access::Read { byte: r.memsize, value: Some(0x3fb0) });
        for (what, bad) in [("left out", gone), ("swapped", swapped), ("another value", other), ("added", extra)] {
            assert!(nbseq::equal(&bad, &r, VRAM).is_err(), "{:?} with an access {what} still equals Linux", good.stage);
        }
        nbseq::equal(good, &r, VRAM).unwrap();
    }
    // An address outside what its rule allows: the GART table "at" a VRAM offset past the card's memory.
    let mut far = done[1].clone();
    let hi = far.trail.iter().position(|a| matches!(a, Access::Write { byte, .. } if *byte == 0x1a5d0 * 4)).unwrap();
    far.trail[hi] = Access::Write { byte: 0x1a5d0 * 4, value: 0x4, reached: true };
    let e = nbseq::equal(&far, &r, VRAM).unwrap_err();
    assert!(e.contains("the rule for it") && e.contains("does not hold"), "{e}");
    // A lost trail proves nothing.
    let mut lost = done[2].clone();
    lost.trail_lost = 1;
    assert!(nbseq::equal(&lost, &r, VRAM).unwrap_err().contains("lost 1 accesses"));
}
