//! Wave 43 (piece 12): the stage `sdmatest` on the model: Linux's IB test on both engines, then the fill, the
//! copy and the trap with VRAM targets, then the fill and the copy across the GART with system pages. Every rung
//! says PASS, what the rungs took is given back, and the stage writes none of the engines' registers. A rung that
//! fails is named in plain words and keeps its memory.

use super::rdna4fix::*;
use super::rdna4sdma::{sr, up};
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::fakemodel::{STAGE_STACK, stack_at};
use crate::rdna4::fakesdmaeng::{Behind, Spoil};
use crate::rdna4::{sdmaih, sdmaseq};
use crate::rdna4::sdmaq::lay;
use crate::rdna4::sdmarungs::{self, COPY_BYTES, FILL_AT, FILL_BYTES, RUNGS, SRC_AT, fill_form, pattern};
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::sdma7;
use std::vec::Vec;

#[test]
fn every_rung_passes_on_the_model_and_everything_taken_is_given_back() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let taken = s.bump.as_ref().unwrap().taken.clone();
    let bound = s.nb.gart.as_ref().unwrap().bound.clone();
    let (flushes, tlb, gc_tlb) = (m.flushes, m.nb.tlb_flushes, m.gc.tlb_flushes);
    m.writes.clear();
    let top = stack_at();
    m.deepest = usize::MAX;
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(d.pass && d.refused.is_empty() && m.refused.is_empty(), "{}\n{}\n{:?}", d.line, d.lines.join("\n"), m.refused);
    assert!(d.line.ends_with("Kopiereinheit der Karte prüfen: fertig."), "{}", d.line);
    // Nine rungs, each with its answer, in the ladder's order.
    let said: Vec<&str> = d.lines.iter().filter_map(|l| l.strip_prefix("rung ")).collect();
    assert_eq!(said.len(), 9);
    for (line, name) in said.iter().zip(RUNGS) {
        assert!(line.starts_with(&std::format!("{name}: PASS: ")), "{line}");
    }
    assert_eq!(s.sdma.rungs, RUNGS.map(|n| (n, true)));
    assert!(said[2].contains("filled as bytes, the pages before and behind untouched: true") && said[6].contains("filled as bytes, its neighbours untouched: true"), "{said:?}");
    assert!(said[3].starts_with("copy-vram: PASS: 8192 of 8192 bytes arrived") && said[7].contains("8192 of 8192") && said[8].contains("8192 of 8192"), "{said:?}");
    // The traps came through the reader the stage ran itself: the two IB tests', six IBs on engine 0, the two
    // trap rungs'. Nothing is left unhandled, and the IH's doorbell told the card how far it was read.
    let ih = s.nb.ih.as_ref().unwrap();
    assert_eq!((sdmaih::traps(ih, 0), sdmaih::traps(ih, 1)), (1 + 5 + 1, 1 + 1));
    assert!(ih.reader.unclaimed.is_empty() && ih.reader.entries[0] == 9 && m.nb.rptr[0] == 9 * 32, "{}", ih.reader.describe());
    assert_eq!((m.sdma.e[0].traps, m.sdma.e[1].traps, m.sdma.e[0].traps_lost + m.sdma.e[1].traps_lost), (7, 2, 0));
    // Each frame flushed the HDP from its ring: the engine saw what the CPU had just written into VRAM.
    assert_eq!(m.sdma.ring_flushes, 9);
    // What the rungs took is back: the VRAM manager and the GART hold what they held, the pool's slots too.
    assert_eq!(s.bump.as_ref().unwrap().taken, taken);
    assert_eq!(s.nb.gart.as_ref().unwrap().bound, bound);
    assert_eq!((s.sdma.held.clone(), s.sdma.up.as_ref().unwrap().pool.used()), ((None, None), 10));
    assert!(d.lines.iter().any(|l| l.starts_with("given back: the test pages' GART entries (unbound and flushed), vram ") && l.ends_with(": yes")), "{:?}", d.lines);
    // The card's walker no longer finds the test pages; the ring's still.
    let p = s.sdma.up.as_ref().unwrap().place;
    assert!(s.nb.gart.as_ref().unwrap().system_of(p.mc + lay::TEST).is_none() && m.sdma.e.iter().all(|e| e.on && e.halted.is_none() && e.rptr == e.wptr));
    // The stage wrote no register of the engines: only the two binds' flushes (map and unmap), each through MES
    // (three HDP flushes each: the bind's own, GFXHUB's, MMHUB's; the hubs' invalidations are MES commands).
    let r = sr(&rig);
    assert!(m.writes.iter().all(|(b, _)| r.reg_at(*b).is_none()), "{:x?}", m.writes);
    assert_eq!((d.totals.writes, m.flushes - flushes - 9, m.nb.tlb_flushes - tlb, m.gc.tlb_flushes - gc_tlb), (6, 6, 2, 2));
    // Its answer ends in its `linuxseq` line: the rungs stand in no section and it says so, and what the trace
    // does have of the call, the flush after the test pages' bind and after their unbind, is Linux's line for line.
    let last = d.lines.last().unwrap();
    assert!(last.starts_with("linuxseq mes: equal: the call's only register accesses are 2 of the TLB flush after a GART change once MES runs") && last.contains("line for line; the rungs themselves stand in no section (Linux's IB tests run from the delayed work"), "{last}");
    assert_eq!(d.lines.iter().filter(|l| l.starts_with("linuxseq")).count(), 1, "{:?}", d.lines);
    assert_eq!((s.before(Stage::SdmaTest), s.passed(Stage::SdmaTest), s.next()), (None, false, None));
    // A stage is no deeper in the stack than the chain's when it reaches the card (an unoptimised host build).
    assert!(top.saturating_sub(m.deepest) <= STAGE_STACK * 3 / 2, "{} bytes deep", top.saturating_sub(m.deepest));
    // A second run does the same: nothing of the first is in its way.
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty() && s.bump.as_ref().unwrap().taken == taken, "{} {:?}", d.line, m.refused);
    assert_eq!(sdmaih::traps(s.nb.ih.as_ref().unwrap(), 0), 14);
}

#[test]
fn the_rungs_frame_an_ib_as_linux_frames_it() {
    let rig = Rig::new();
    let (mut s, m, _) = up(&rig);
    let (d, mut m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(d.pass, "{}", d.line);
    let up = s.sdma.up.as_ref().unwrap();
    // Engine 1 ran two submissions after its ring test: the IB test at dword 16, the trap rung behind it.
    let mut ring = [0u8; 32 * 4];
    assert!(crate::rdna4::host::Host::sys_read(&mut m, up.place.sys + lay::ring(1) + 16 * 4, &mut ring));
    let w: Vec<u32> = ring.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    let (cond, fence) = (up.place.slot_mc(up.q[1].slots.cond_exe) as u32, up.place.slot_mc(up.q[1].slots.fence) as u32);
    let (done, req) = sr(&rig).hdp_flush_dwords().unwrap();
    assert_eq!((done, req), (0xE27, 0xE26));
    // amdgpu_ib_schedule with no job: COND_EXE on the ring's slot with the count of everything behind it, the
    // HDP flush (func 3, done and request registers as byte offsets, engine 1's bit, retry 0xfff interval 10),
    // NOPs so the IB packet ends on an 8-dword boundary, INDIRECT in VMID 0, FENCE (UC) of sequence 1, TRAP.
    assert_eq!(&w[..5], [0x9, cond, 0, 1, 6 + 7 + 6 + 4 + 2]);
    assert_eq!(&w[5..11], [0x3400_0008, done << 2, req << 2, 0x800, 0x800, 0x0FFF_000A]);
    // The ring stood at dword 16: (2 - (16 + 5 + 6)) & 7 = 7 NOPs, and the IB packet ends at dword 40.
    assert_eq!(&w[11..18], [0; 7]);
    assert_eq!(&w[18..24], [0x4, up.place.ib_mc() as u32, 0, 8, 0, 0]);
    assert_eq!(&w[24..30], [0x0003_0005, fence, 0, 1, 0x6, 0]);
    assert_eq!(&w[30..32], [0; 2], "the commit's pad to the fetch size");
    // The IB page still holds the last IB of engine 0: Linux's copy of eight dwords, system pages to VRAM.
    let mut ib = [0u8; 32];
    assert!(crate::rdna4::host::Host::sys_read(&mut m, up.place.sys + lay::IB, &mut ib));
    let ib: Vec<u32> = ib.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    assert_eq!((ib[0], ib[1], ib[2], ib[7]), (0x0008_0001, 8191, 0, 0));
    assert_eq!(up.q[1].seq, 2);
    assert_eq!(up.q[0].seq, 7);
}

#[test]
fn the_stage_needs_the_engines_and_the_ih_of_this_boot_and_asks_before_any_access() {
    let rig = Rig::new();
    // No `sdma` in this boot.
    let (mut s, m) = super::rdna4sdma::loaded(&rig);
    let taken = s.bump.as_ref().unwrap().taken.len();
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && d.line.contains("zuerst die Kopiereinheit der Karte starten"), "{}", d.line);
    assert_eq!((s.bump.as_ref().unwrap().taken.len(), s.sdma.held.clone()), (taken, (None, None)), "nothing was taken for a stage that does not run");
    let _ = m;
    // No IH (its state gone): nothing to arrive through.
    let (mut s, mut m, _) = up(&rig);
    let ih = s.nb.ih.take();
    m.writes.clear();
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && m.sdma.doorbells.len() == 2 && d.line.contains("zuerst die Meldungen der Karte einschalten"), "{}", d.line);
    s.nb.ih = ih;
    // The second card.
    let mut second = Rig::new();
    second.display = false;
    let (d, m) = second.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && m.sdma.doorbells.len() == 2 && d.line.contains("die zweite Karte bleibt unberührt"), "{}", d.line);
}

/// A trap that does not arrive: the IB test still passes on its fence, as Linux's does when only the fallback
/// timer signals it; the trap's own rung does not, and says what is missing in plain words.
#[test]
fn a_trap_that_does_not_arrive_fails_its_own_rung_and_keeps_what_was_taken() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    let cntl = sr(&rig).byte(sdma7::CNTL, 0).unwrap();
    assert!(m.reg_write(cntl, 0x2440));
    let t0 = m.card.now_us;
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("angehalten: die Meldung der Kopiereinheit ist nicht angekommen"), "{}", d.line);
    assert_eq!(s.sdma.rungs, [("ib-test-0", true), ("ib-test-1", true), ("fill-vram", true), ("copy-vram", true), ("trap-0", false)]);
    assert!(d.lines.iter().any(|l| l.starts_with("rung ib-test-0: PASS: ") && l.ends_with("its trap came through the IH reader: false")), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.starts_with("rung trap-0: FAIL: fence 4 after ") && l.contains("(client 10, source 49, ring id 0) came through the IH reader: false")), "{:?}", d.lines);
    // Each wait for a trap was bounded: 1000 polls 100 us apart behind the fence, four times.
    let waited = m.card.now_us - t0;
    assert!((4 * 1000 * 100..5 * 1000 * 100).contains(&waited), "{waited} us");
    assert!(d.trail.iter().all(|a| !matches!(a, Access::Sleep { .. })), "a poll's steps are no sleeps");
    // The VRAM range stays taken (an engine may still write there), and the answer says so.
    assert!(s.sdma.held.0.is_some() && s.bump.as_ref().unwrap().taken.iter().any(|t| t.0 == "SDMA test ranges"));
    assert!(d.lines.iter().any(|l| l.starts_with("kept after the failed rung (an engine may still write there): vram ")), "{:?}", d.lines);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    // The next run uses the same range again and takes no second one.
    let mut m = m;
    assert!(m.reg_write(cntl, 0x2441));
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty() && s.sdma.held == (None, None), "{} {:?}", d.line, d.lines);
    assert!(s.bump.as_ref().unwrap().taken.iter().all(|t| t.0 != "SDMA test ranges"));
}

/// An engine that never runs its ring: the fence does not come within Linux's second, and the stage says which
/// engine and where to look.
#[test]
fn a_fence_that_does_not_come_ends_the_stage_within_its_bound() {
    let rig = Rig::new();
    let (mut s, mut m, _) = up(&rig);
    m.sdma.dead[1] = true;
    let t0 = m.card.now_us;
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("die Kopiereinheit hat einen Auftrag nicht beendet"), "{}", d.line);
    assert!(d.lines.last().unwrap().contains("engine 1: fence 1 did not come within 10000 polls 100 us apart (the slot reads 0)"), "{:?}", d.lines.last());
    // 10000 polls 100 us apart: Linux's second.
    assert!((999_000..1_100_000).contains(&(m.card.now_us - t0)), "{} us", m.card.now_us - t0);
    assert_eq!(s.sdma.rungs, [("ib-test-0", true)]);
    // The slot the IB test took was given back all the same.
    assert_eq!(s.sdma.up.as_ref().unwrap().pool.used(), 10);
}

/// The engine told to copy from VRAM the stage's guard does not list is not this test's business; that the stage
/// itself may write only its own VRAM range and system memory is: nothing was refused on a good run, and the
/// lists hold exactly the range, the table, the piece and the IH's write-back page.
#[test]
fn the_stages_lists_hold_its_own_memory_and_doorbells_only() {
    let rig = Rig::new();
    let (mut s, m, _) = up(&rig);
    let n = crate::rdna4::nbregs::NbRegs::from(&rig.att.discovery, &super::rdna4nb::regs(&rig)).unwrap();
    let allow = crate::rdna4::sdmastage::allow(Stage::SdmaTest, &mut s, &n, Some(&sr(&rig)));
    let l = s.layout.clone().unwrap();
    let held = s.sdma.held.0.clone().unwrap();
    assert_eq!(allow.mem, [s.nb.gart.as_ref().unwrap().table_mc(), l.mc(held.start)..l.mc(held.end)]);
    assert_eq!((held.end - held.start, allow.regs.len()), (sdmarungs::VRAM_BYTES, 1 + 2 + 2), "the flush's HDP flush register (through MES the hubs' requests are the firmware's), the IH rings' two controls and the two hubs' fault controls (the reader's clear)");
    assert!(allow.regs.iter().all(|b| sr(&rig).reg_at(*b).is_none()), "no register of the engines");
    // Asking a second time takes no second range.
    let again = crate::rdna4::sdmastage::allow(Stage::SdmaTest, &mut s, &n, Some(&sr(&rig)));
    assert_eq!((again.mem, s.bump.as_ref().unwrap().taken.iter().filter(|t| t.0 == "SDMA test ranges").count()), (allow.mem.clone(), 1));
    let mut g = crate::rdna4::sdmastage::guarded(crate::rdna4::guard::Guard::new(m, allow, super::rdna4nb::regs(&rig)), Stage::SdmaTest, &s);
    let (sys, ih) = (s.sdma.piece.unwrap(), s.nb.ih.as_ref().unwrap().place.sys);
    use crate::rdna4::host::Host;
    // Its own: the piece with the test pages, the IH's write-back page, both blocks' doorbells.
    assert!(g.sys_write(sys + lay::TEST, &[1]) && g.sys_write(ih + 2 * (256 << 10) + 4, &[0; 4]));
    // Not its own: the IH rings' entries, another dword of BAR2, VRAM beside its range, new pages.
    assert!(!g.sys_write(ih, &[0]) && !g.doorbell_write64(0x216, 0) && !g.mem_write(l.mc(held.end), &[0]) && g.pages(4096).is_none());
    assert_eq!(g.refused.len(), 4);
}

/// An engine that gets its data wrong: each rung's own comparison finds it, says FAIL with what it read, stops
/// the stage in plain words and keeps what the rungs took. One case per comparison of the ladder.
#[test]
fn a_rung_whose_data_is_wrong_fails_in_plain_words_and_keeps_what_was_taken() {
    use Behind::{Sys, Vram};
    const P: u64 = 4096;
    let (wrong, filled, copied) = ("die Kopiereinheit hat einen Auftrag falsch ausgeführt", "die Kopiereinheit hat falsch gefüllt", "die Kopiereinheit hat falsch kopiert");
    // The rung, how the engine errs (given the VRAM range's offset, the test pages and the IB test's slot), the
    // stage's plain word, and what the rung's line says. A byte that lands is 0, where every pattern is odd.
    type Err = fn(u64, u64, u64) -> Spoil;
    let cases: [(usize, Err, &str, &str); 9] = [
        (0, |_, _, slot| Spoil::Also { when: Sys(slot), bytes: 4, at: Sys(slot), data: std::vec![0] }, wrong, "the IB's slot reads 0xdeadbe00 (want 0xdeadbeef)"),
        // The fill: a byte of the page behind it, of the page before it, and a byte of the fill itself.
        (2, |v, _, _| Spoil::Also { when: Vram(v + FILL_AT), bytes: FILL_BYTES, at: Vram(v + FILL_AT + FILL_BYTES), data: std::vec![0] }, filled, "filled as bytes, the pages before and behind untouched: false"),
        (2, |v, _, _| Spoil::Also { when: Vram(v + FILL_AT), bytes: FILL_BYTES, at: Vram(v + FILL_AT - 4), data: std::vec![0] }, filled, "filled as bytes, the pages before and behind untouched: false"),
        (2, |v, _, _| Spoil::Also { when: Vram(v + FILL_AT), bytes: FILL_BYTES, at: Vram(v + FILL_AT + 5), data: std::vec![0] }, filled, "filled as neither bytes nor dwords of the value, the pages before and behind untouched: true"),
        (3, |v, _, _| Spoil::Misread(Vram(v + SRC_AT + 100)), copied, "8191 of 8192 bytes arrived from vram mc"),
        (6, |_, t, _| Spoil::Also { when: Sys(t + P), bytes: P, at: Sys(t + 2 * P), data: std::vec![0] }, filled, "filled as bytes, its neighbours untouched: false"),
        (6, |_, t, _| Spoil::Also { when: Sys(t + P), bytes: P, at: Sys(t + P + 7), data: std::vec![0] }, filled, "filled as neither bytes nor dwords of the value, its neighbours untouched: true"),
        (7, |_, t, _| Spoil::Also { when: Sys(t + 2 * P), bytes: COPY_BYTES, at: Sys(t + 3 * P), data: std::vec![0] }, copied, "8191 of 8192 bytes arrived from vram mc"),
        (8, |_, t, _| Spoil::Misread(Sys(t + 2 * P + 9)), copied, "8191 of 8192 bytes arrived from system pages at mc"),
    ];
    for (n, (rung, spoil, plain, said)) in cases.into_iter().enumerate() {
        let rig = Rig::new();
        let (mut s, mut m, _) = up(&rig);
        // The range the stage is going to take (it takes it before its guard exists, and uses it again).
        sdmarungs::take_vram(&mut s).unwrap();
        let v = s.sdma.held.0.clone().unwrap().start;
        let up = s.sdma.up.as_mut().unwrap();
        let slot = up.pool.get().unwrap();
        up.pool.free(slot).unwrap();
        let (slot, t) = (up.place.slot_sys(slot), up.place.sys + lay::TEST);
        m.sdma.spoil = Some(spoil(v, t, slot));
        let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
        let name = RUNGS[rung];
        assert!(!d.pass && d.line.ends_with(&std::format!("angehalten: {plain}.")), "case {n}: {}\n{}", d.line, d.lines.join("\n"));
        // Every rung before it passed, this one did not, none after it ran.
        let want: Vec<(&str, bool)> = RUNGS[..rung].iter().map(|r| (*r, true)).chain([(name, false)]).collect();
        assert_eq!(s.sdma.rungs, want, "case {n}");
        let line = d.lines.iter().find(|l| l.starts_with(&std::format!("rung {name}: FAIL: "))).unwrap_or_else(|| panic!("case {n}: {:?}", d.lines));
        assert!(line.contains(said), "case {n}: {line}");
        assert!(d.lines.last().unwrap().starts_with(&std::format!("stopped: rung {name}: ")), "case {n}: {:?}", d.lines.last());
        // The line before it holds the call against the trace as far as it came: the bind's flush, from the
        // first rung across the GART on. A call that stopped before that made no register access and has none.
        let seq: Vec<&str> = d.lines.iter().map(|l| l.as_str()).filter(|l| l.starts_with("linuxseq")).collect();
        if rung >= 6 {
            assert!(seq.len() == 1 && d.lines[d.lines.len() - 2].starts_with("linuxseq mes: the same as Linux's as far as the stage came: the call's only register accesses are 1 of the TLB flush after a GART change once MES runs"), "case {n}: {seq:?}");
        } else {
            assert!(seq.is_empty() && d.trail.is_empty(), "case {n}: {seq:?}");
        }
        // Kept: the VRAM range, and once they were bound the test pages, which the card's walker still finds.
        assert!(s.sdma.held.0.is_some() && s.bump.as_ref().unwrap().taken.iter().any(|t| t.0 == "SDMA test ranges"), "case {n}");
        let kept = d.lines.iter().find(|l| l.starts_with("kept after the failed rung (an engine may still write there): vram ")).unwrap_or_else(|| panic!("case {n}: {:?}", d.lines));
        assert_eq!((s.sdma.held.1.is_some(), kept.contains(", the test pages bound at mc ")), (rung >= 6, rung >= 6), "case {n}: {kept}");
        if let Some(mc) = s.sdma.held.1 {
            assert_eq!(s.nb.gart.as_ref().unwrap().system_of(mc), Some(t), "case {n}");
        }
        // The engine itself ran every packet: nothing was refused and none stopped.
        assert!(m.refused.is_empty() && m.sdma.e.iter().all(|e| e.halted.is_none()), "case {n}: {:?}", m.refused);
    }
}

/// The stage's `linuxseq` line holds every register access of a call against Linux's flush after a GART change:
/// a flush cut short, a line of it changed, or an access of another kind is named as the first difference.
#[test]
fn the_rungs_line_names_a_flush_that_is_not_linuxs() {
    let rig = Rig::new();
    let (mut s, m, _) = up(&rig);
    let (d, _) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(d.pass && d.trail_lost == 0, "{}", d.line);
    let regs = super::rdna4nb::regs(&rig);
    let line = |pass: bool, t: &[Access], lost: u64| sdmaseq::rungs_verdict(pass, t, lost, &regs, &[]);
    // The trail as it was: two flushes. Nothing at all on a call that was refused before any access.
    assert!(line(true, &d.trail, 0).unwrap().starts_with("linuxseq mes: equal: the call's only register accesses are 2 of"));
    assert_eq!(line(false, &[], 0), None);
    // A call that passed unbound its test pages: one with no flush at all is none of Linux's.
    assert!(line(true, &[], 0).unwrap().starts_with("linuxseq mes: differs: the call passed with no register access: the TLB flush after the test pages' unbind is missing"));
    // Through MES a flush is three HDP flushes, each a write and a read: six accesses.
    assert_eq!(d.trail.len(), 12);
    let half = d.trail.len() / 2;
    // The unbind's flush cut short by its last HDP flush: on a passed call Linux has a line left; on a stopped
    // one it is as far as it came.
    let cut = &d.trail[..d.trail.len() - 2];
    let said = line(true, cut, 0).unwrap();
    assert!(said.starts_with("linuxseq mes: differs: mes.txt line ") && said.contains("Linux goes on and flushes the HDP") && said.contains("the stage did nothing more"), "{said}");
    assert!(line(false, cut, 0).unwrap().starts_with("linuxseq mes: the same as Linux's as far as the stage came: the call's only register accesses are 2 of"));
    // The bind's flush without its first HDP flush.
    let no_hdp = &d.trail[2..half];
    assert!(line(true, no_hdp, 0).unwrap().starts_with("linuxseq mes: differs: mes.txt line "), "{:?}", line(true, no_hdp, 0));
    // A register write that is not Linux's flush (here: another value into the flush register).
    let mut other = d.trail.clone();
    let w = other.iter().rposition(|a| matches!(a, Access::Write { .. })).unwrap();
    if let Access::Write { value, .. } = &mut other[w] {
        *value ^= 0x10;
    }
    let said = line(true, &other, 0).unwrap();
    assert!(said.starts_with("linuxseq mes: differs: mes.txt line ") && said.contains("Linux flushes the HDP"), "{said}");
    let mut extra = d.trail[..half].to_vec();
    extra.push(Access::Read { byte: 0x4000, value: Some(0) });
    assert!(line(true, &extra, 0).unwrap().starts_with("linuxseq mes: differs: "), "{:?}", line(true, &extra, 0));
    // The same access at a register of Linux's interrupt handlers (the stage's own IH reader took an overflow or
    // a page fault while it waited): taken out, counted and said; without that list it is a difference like any.
    let handlers = crate::rdna4::sdmastage::irq_regs(&super::rdna4nbmodel::n(&rig));
    assert_eq!(handlers.len(), 8);
    let mut with = d.trail[..half].to_vec();
    with.push(Access::Read { byte: handlers[4], value: Some(0x123) });
    with.extend_from_slice(&d.trail[half..]);
    let said = sdmaseq::rungs_verdict(true, &with, 0, &regs, &handlers).unwrap();
    assert!(said.starts_with("linuxseq mes: equal: the call's only register accesses are 2 of") && said.contains("; 1 accesses of Linux's interrupt handlers"), "{said}");
    assert!(line(true, &with, 0).unwrap().starts_with("linuxseq mes: differs: "));
    assert!(!line(true, &d.trail, 0).unwrap().contains("interrupt handlers"));
    // A trail that lost accesses proves nothing.
    assert!(line(true, &d.trail, 3).unwrap().contains("differs: the stage's trail lost 3 accesses"));
    // Every form of the line says why the rungs themselves are compared with nothing.
    for l in [line(true, &d.trail, 0), line(true, &[], 0), line(true, cut, 0), line(true, &d.trail, 3)] {
        assert!(l.unwrap().ends_with("; the rungs themselves stand in no section (Linux's IB tests run from the delayed work 2 s after init, amdgpu_device.c:3005-3011)"));
    }
}

#[test]
fn the_fills_form_and_the_patterns() {
    assert_eq!(fill_form(&[0xA5; 64]), Some("bytes"));
    let dwords: Vec<u8> = (0..16).flat_map(|_| 0xA5u32.to_le_bytes()).collect();
    assert_eq!(fill_form(&dwords), Some("dwords"));
    let mut one_off = [0xA5u8; 64];
    one_off[63] = 0xA4;
    assert_eq!((fill_form(&one_off), fill_form(&[0u8; 64])), (None, None));
    // A pattern has no zero dword and no two alike, so a copy that lands shifted or a fill that leaks shows.
    let p = pattern(0x2222_0000, 8192);
    let mut d: Vec<u32> = p.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    assert!(d.iter().all(|x| *x != 0) && p.len() == 8192);
    d.sort_unstable();
    d.dedup();
    assert!(d.len() >= 2048 - 1, "{} distinct dwords", d.len());
    assert_ne!(pattern(0x2222_0000, 64), pattern(0x4444_0000, 64));
}

/// A session after the secure processor's ring was destroyed keeps the engines: the rungs still run.
#[test]
fn the_engines_outlive_the_secure_processors_ring() {
    let rig = Rig::new();
    let (mut s, m, _) = up(&rig);
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy]);
    assert!(s.sdma.up.is_some() && s.passed(Stage::Sdma) && s.next() == Some(Stage::Ring));
    let (d, m) = rig.run(&mut s, Stage::SdmaTest, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty(), "{} {:?}", d.line, m.refused);
    let _: &Session = &s;
}
