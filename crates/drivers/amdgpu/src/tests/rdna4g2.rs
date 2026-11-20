//! GP-GPU2: the card's stages against the model that refuses what Linux would not do (`rdna4::fakemodel`): Linux's
//! order passes and writes nothing else; the window stays out of the picture and below the firmware's region; the
//! SMU is only asked; the second card stays untouched; a stale ring, a hanging fence, a refused SMU and the
//! firmware list's order and types; the note keyed by address.

use super::rdna4fix::*;
use crate::gpucore::psp;
use crate::rdna4::fakemodel::MC_BASE;
use crate::rdna4::fwlist::{self, Id};
use crate::rdna4::guard::Refusal;
use crate::rdna4::pspfiles::{self, SosFile, TaFile};
use crate::rdna4::stage::{Session, Stage};
use crate::rdna4::vram::{self, GUARD, VramStop, WINDOW};
use rdna4_core::card::CARD_A;
use rdna4_core::hub41::{self, LayoutStop};
use std::vec::Vec;

#[test]
fn linux_order_passes_on_the_model_and_nothing_else_is_written() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut stick = Stick::full();
    let mut stages = std::vec![Stage::Alive, Stage::Bootloader, Stage::Smu];
    stages.extend(ORDER);
    stages.extend([Stage::State, Stage::Destroy]);
    let (done, m) = rig.cold_pass(&mut s, model(), &mut stick, &stages);
    for d in &done {
        assert!(d.line.ends_with(": fertig."), "{}", d.line);
    }
    assert_eq!(m.next(), None, "every command of Linux's sequence was answered");
    assert_eq!(m.commands.len(), 31, "Linux's commands for 14.0.3: no SETUP_TMR");
    assert_eq!(m.flushes as usize, m.commands.len() + 9, "one HDP flush per frame, as amdgpu_psp.c:3961, and one per bootloader part");
    assert!(m.ring.is_none(), "destroyed at the end");
    // The read-only stages wrote nothing; the SMU stage only the index pair and the mailbox.
    for d in &done[..2] {
        assert_eq!(d.totals.writes, 0, "{:?}", d.stage);
    }
    assert_eq!(done[2].totals.writes, 1 + 3 * 12, "the index pair's address, three writes per question");
    assert_eq!(done[6].totals.writes, 0, "tmr sends nothing");
    // The ring's stages: create's four registers, then a flush and a write pointer per command.
    assert_eq!(done[4].totals.writes, 4 + 2 * 2);
    assert_eq!(done[7].totals.writes, 2 * 20, "fw: the SMU's firmware and nineteen images");
    assert_eq!((done[8].totals.writes, done[9].totals.writes, done[10].totals.writes), (2, 4, 10));
    assert_eq!(m.messages.len(), 12);
    assert!(m.messages.iter().all(|(id, _)| rdna4_core::refuse::write_refused(rdna4_core::regmap::SMU_MESSAGE, u32::from(*id)).is_none()));
    // What the session knows at the end of the chain.
    let (s, _) = rig.up_to(&ORDER);
    assert_eq!(s.loaded.len(), 22);
    assert_eq!((s.loaded[0].0, s.loaded[19].0, s.loaded[20].0, s.loaded[21].0), (Id::Smc, Id::RlcG, Id::Vcn, Id::Dmcub));
    assert_eq!(s.tas.iter().map(|t| (t.name, t.status)).collect::<Vec<_>>(), [("ASD", 0), ("RAS", 0), ("HDCP", 0), ("DTM", 0)]);
    assert!(s.autoload && s.rest_done && s.ta_done && s.fw_done);
}

#[test]
fn the_window_stays_out_of_the_picture_and_below_the_firmwares_region() {
    let rig = Rig::new();
    let (s, m) = rig.up_to(&[Stage::Sos, Stage::Ring]);
    let l = s.layout.clone().unwrap();
    assert_eq!(l.mc_base, MC_BASE);
    assert_eq!(l.fb, Some(0..PICTURE));
    assert!(l.window.start >= PICTURE + GUARD && l.window.end - l.window.start == WINDOW);
    let r = s.reserved.unwrap();
    // What Linux reserves after these answers: 50 MiB at the top, 4 MiB more (the trace's buffer events).
    assert_eq!((r[0].size, r[0].base + u64::from(r[0].size), r[1].size), (50 << 20, 16304 << 20, 4 << 20));
    assert!(l.window.end <= r[0].base && l.window.end <= r[1].base, "{r:?}");
    // The firmware buffer is the window's first megabyte boundary, the ring's three pages follow it.
    let given = s.given();
    assert_eq!(given.len(), 4);
    assert!(given[0].start.is_multiple_of(1 << 20) && given[0].end - given[0].start == 1 << 20 && s.pri_mc() == Some(l.mc(given[0].start)));
    assert!(given.iter().all(|g| g.start >= l.window.start && g.end <= l.window.end));
    assert!(m.refused.is_empty(), "{:?}", m.refused);

    // A firmware region that reaches down into the window: the ring goes again and the stage stops.
    let (mut s, mut big) = rig.up_to(&[Stage::Sos]);
    big.reserved_at = Some(0x0300_0000);
    let (d, m) = rig.run(&mut s, Stage::Ring, big, &mut Stick::full());
    assert!(!d.pass && d.line.contains("gehört der Karte selbst"), "{}", d.line);
    assert!(m.ring.is_none() && m.refused.is_empty() && !s.ring_up());
}

#[test]
fn the_window_rules() {
    let bar0 = crate::rdna4::attach::BarFact { phys: CARD_A.bar0_base, bytes: CARD_A.bar0_bytes, is64: true, prefetch: true };
    let vram = 16304u64 << 20;
    let gop = Some(crate::attach::Gop { phys: CARD_A.bar0_base, bytes: PICTURE });
    // The way `gmc` goes (`gmcstage::plan`): as Linux, the location base and the memory size place VRAM
    // (`hub41::layout`; the location's top is never read), then the window is placed in it (`vram::place`).
    let plan = |memsize_mib: u32, base: u32, gop| hub41::layout(memsize_mib, base, 0).map(|h| vram::place(h.vram_bytes, h.vram_start, bar0, gop));
    let l = plan(16304, 0x8000, gop).unwrap().unwrap();
    assert_eq!((l.vram_bytes, l.mc_base, l.window.start), (vram, 0x80_0000_0000, 0x0120_0000));
    // A picture on another card: the window starts after the guard.
    let l = plan(16304, 0x8000, Some(crate::attach::Gop { phys: 0x4000_0000, bytes: PICTURE })).unwrap().unwrap();
    assert_eq!((l.fb, l.window.start), (None, GUARD));
    // A hub that does not answer, in either register, and a memory size that is none.
    assert_eq!(plan(16304, u32::MAX, gop).unwrap_err(), LayoutStop::Hub { base: u32::MAX, offset: 0 });
    assert_eq!(hub41::layout(16304, 0x8000, u32::MAX), Err(LayoutStop::Hub { base: 0x8000, offset: u32::MAX }));
    assert!(matches!(hub41::layout(0, 0x8000, 0), Err(LayoutStop::Memsize(0))) && matches!(hub41::layout(u32::MAX, 0x8000, 0), Err(LayoutStop::Memsize(_))));
    // A base from which VRAM would run into the hole of the card's address space: the highest base that still
    // fits 1 GiB below it passes, the same base with this card's VRAM does not.
    let top = ((hub41::MC_HOLE_START - (1 << 30)) >> 24) as u32;
    assert!(plan(1024, top, gop).unwrap().is_ok());
    assert!(matches!(plan(1024, top + 1, gop), Err(LayoutStop::VramPastHole { .. })) && matches!(plan(16304, top, gop), Err(LayoutStop::VramPastHole { .. })));
    // A card too small for the window below its half.
    assert!(matches!(plan(64, 0x8000, gop).unwrap(), Err(VramStop::NoRoom { .. })));
}

#[test]
fn the_second_card_stays_untouched() {
    let mut rig = Rig::new();
    rig.display = false;
    for st in [Stage::Alive, Stage::Smu, Stage::Sos, Stage::Ring, Stage::Ta, Stage::Destroy] {
        let mut s = Session::default();
        let (d, m) = rig.run(&mut s, st, model(), &mut Stick::full());
        assert!(!d.pass && d.totals.writes == 0 && m.writes.is_empty(), "{st:?}");
        assert!(d.line.starts_with("Grafikkarte 1: ") && d.line.contains("unberührt"), "{}", d.line);
        assert!(s.layout.is_none() && s.boot_sent() == 0);
    }
    let (d, _) = rig.run(&mut Session::default(), Stage::State, model(), &mut Stick::full());
    assert!(d.pass && d.totals.writes == 0);
}

#[test]
fn a_ring_from_before_is_destroyed_first_and_a_note_lets_the_next_ring_through() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut stick = Stick::full();
    // A card whose secure OS runs, with a ring a kernel before left: the load stage sends nothing.
    let (_, m) = rig.cold_pass(&mut s, model().stale_ring(), &mut stick, &[Stage::Sos]);
    assert!(m.writes.is_empty());
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(!d.pass && d.line.contains("erst abbauen") && d.totals.writes == 0 && m.writes.is_empty(), "{}", d.line);
    let (d, m) = rig.run(&mut s, Stage::Destroy, m, &mut stick);
    assert!(d.pass && m.ring.is_none() && m.refused.is_empty(), "{:?} {:?}", d.lines, m.refused);
    let n = d.note.unwrap();
    assert_eq!(n.ring, crate::gpucore::handoff::RingFate::Destroyed);
    // The PSP keeps C2PMSG_71 after a destroy: the note of this boot says it is gone, so the next ring is made.
    assert_ne!(m.c71, 0);
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(d.pass && m.ring.is_some(), "{:?}", d.lines);
    // A kernel after a Reload has no session: the inherited note does the same.
    let mut rig2 = Rig::new();
    let (d, m) = rig.run(&mut s, Stage::Destroy, m, &mut stick);
    rig2.inherited = d.note;
    let mut s2 = Session::default();
    let (_, m) = rig2.cold_pass(&mut s2, m, &mut stick, &[Stage::Sos, Stage::Ring]);
    assert!(m.ring.is_some());
    // The ring of a secure OS this boot loaded: after its destroy the next ring stage looks before it writes.
    let (mut s, m) = rig.up_to(&[Stage::Sos, Stage::Ring]);
    let (_, m) = rig.pass(&mut s, m, &mut stick, &[Stage::Destroy]);
    assert!(matches!(s.sos, Some(crate::rdna4::stage::Sos::Warm { .. })) && s.boot_sent() == 9 && s.given().is_empty());
    let (done, m) = rig.pass(&mut s, m, &mut stick, &[Stage::Ring]);
    let p = m.regs.psp;
    let first: Vec<u32> = done[0].trail.iter().take(4).map(|a| match a {
        crate::gpucore::trail::Access::Read { byte, .. } => *byte,
        _ => 0,
    }).collect();
    assert_eq!(first, [p.c2pmsg_67, p.c2pmsg_71, p.c2pmsg_69, p.c2pmsg_70], "the registers of the ring before are read first");
}

#[test]
fn a_fence_that_never_comes_wedges_the_ring_and_only_destroy_goes_on() {
    let rig = Rig::new();
    let mut stick = Stick::full();
    let (mut s, mut m) = rig.up_to(&[Stage::Sos, Stage::Ring]);
    m.hang_on = Some(psp::cmd::LOAD_TOC);
    let t0 = m.card.now_us;
    let (d, m) = rig.run(&mut s, Stage::Toc, m, &mut stick);
    assert!(!d.pass && d.line.contains("nicht beantwortet"), "{}", d.line);
    assert!(m.card.now_us - t0 <= 2_100_000, "bounded by Linux's 2 s");
    let (d, m) = rig.run(&mut s, Stage::Tmr, m, &mut stick);
    assert!(!d.pass && d.totals.writes == 0, "{}", d.line);
    let (d, m) = rig.run(&mut s, Stage::Destroy, m, &mut stick);
    assert!(d.pass && m.ring.is_none(), "{:?}", d.lines);
    // A ring stage that fails after its create still hands on that the ring is up, with the pointer it wrote.
    let (mut s, mut m) = rig.up_to(&[Stage::Sos]);
    m.hang_on = Some(0x50);
    let (d, _) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(!d.pass && d.note.map(|n| (n.ring, n.wptr)) == Some((crate::gpucore::handoff::RingFate::NotDestroyed, 0x10)), "{:?}", d.note);
}

/// Linux reloads the SMU's firmware at a cold start of this card (amdgpu_psp.c:3521-3526; the trace's fourth
/// frame): it is the first image, always, and a card that expects another first refuses.
#[test]
fn the_smus_firmware_is_loaded_first_always() {
    let rig = Rig::new();
    let (s, m) = rig.up_to(&ORDER[..5]);
    assert_eq!(s.loaded.first().map(|l| l.0), Some(Id::Smc));
    assert_eq!((s.loaded.len(), s.loaded.last().map(|l| l.0), m.commands[3]), (20, Some(Id::RlcG), psp::cmd::LOAD_IP_FW));
    // A model whose secure OS expects no SMU firmware (what BitsOS sent before): it refuses the first image.
    let (mut s, mut m) = rig.up_to(&ORDER[..4]);
    m.expect.images.remove(0);
    m.expect.before_autoload -= 1;
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(!d.pass && m.refused.iter().any(|r| r.contains("type 18 where Linux loads type 71")), "{:?}", m.refused);
}

/// A stage that stopped after some of its commands were answered does not send those again: only a new ring
/// (after destroy) starts over.
#[test]
fn a_stage_that_stopped_half_way_is_not_run_again() {
    let rig = Rig::new();
    // The display's firmware is not what the card expects: VCN is taken, DMCUB refused.
    let (mut s, mut m) = rig.up_to(&ORDER[..6]);
    m.expect.images[21].1[0] ^= 0xFF;
    let (d, m) = rig.run(&mut s, Stage::FwRest, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("lehnt einen Befehl ab") && s.loaded.len() == 21, "{}", d.line);
    let n = m.commands.len();
    let (again, m) = rig.all(&mut s, m, &mut Stick::full(), &[Stage::FwRest, Stage::Ta, Stage::Fw]);
    assert!(again.iter().all(|d| !d.pass && d.totals.writes == 0) && m.commands.len() == n);
    // The same in the middle of fw.
    let (mut s, mut m) = rig.up_to(&ORDER[..4]);
    m.expect.images[5].1[0] ^= 0xFF;
    let (d, m) = rig.run(&mut s, Stage::Fw, m, &mut Stick::full());
    assert!(!d.pass && s.loaded.len() == 5 && !s.fw_done, "{}", d.line);
    let n = m.commands.len();
    let (again, m) = rig.all(&mut s, m, &mut Stick::full(), &[Stage::Fw, Stage::Autoload, Stage::FwRest]);
    assert!(again.iter().all(|d| !d.pass && d.totals.writes == 0) && m.commands.len() == n);
    // Destroy, and the chain runs again from the ring on a card whose secure OS runs.
    let mut m = m;
    assert_eq!(m.refused, ["LOAD_IP_FW type 90: not that image's bytes"]);
    m.refused.clear();
    m.expect = expect();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy]);
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &ORDER[1..]);
    assert_eq!((m.next(), m.boot.taken, s.boot_sent()), (None, 9, 9));
}

/// A trusted application the secure OS does not accept is Linux's to log and pass over (amdgpu_psp.c:3645-3670):
/// the stage says so and goes on to the next; the ASD and the register list are not optional.
#[test]
fn an_optional_trusted_application_that_is_refused_does_not_stop_the_stage() {
    let rig = Rig::new();
    for n in 0..3 {
        let (mut s, mut m) = rig.up_to(&ORDER[..7]);
        m.ta_status = Some((n, 0x13));
        let (done, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Ta]);
        assert_eq!(s.tas.iter().map(|t| t.status).collect::<Vec<_>>(), (0..4).map(|i| if i == n + 1 { 0x13 } else { 0 }).collect::<Vec<_>>());
        assert_eq!(done[0].lines.iter().filter(|l| l.contains("not accepted; Linux logs that and goes on")).count(), 1, "{:?}", done[0].lines);
        assert_eq!((m.commands.len(), m.next()), (31, None));
    }
    // The shared buffers lie in the window, a page-aligned 16 KiB each, and RAS's holds Linux's init flags.
    let (s, m) = rig.up_to(&ORDER);
    let l = s.layout.clone().unwrap();
    let shared: Vec<_> = s.given().iter().filter(|g| g.end - g.start == 0x4000).collect();
    assert_eq!(shared.len(), 3);
    assert!(shared.iter().all(|g| g.start.is_multiple_of(0x1000) && g.start >= l.window.start && g.end <= l.window.end));
    let mut flags = [0u8; 16];
    assert!(crate::gpucore::psp::Hw::mem_read(&mut { m }, shared[0].start + 16, &mut flags));
    assert_eq!(flags, RAS_FLAGS);
    // A file without one of the five: the stage stops before the first of them is sent.
    for kind in [pspfiles::TA_ASD, pspfiles::TA_RAS, pspfiles::TA_HDCP, pspfiles::TA_DTM] {
        let (mut s, m) = rig.up_to(&ORDER[..7]);
        let mut stick = Stick::full();
        let b = stick.0.get_mut(&crate::gpucore::fwload::stick_path(fwlist::TA)).unwrap();
        let at = (0..4).map(|i| 36 + i * 16).find(|at| b[*at..*at + 4] == kind.to_le_bytes()).unwrap();
        b[at + 12..at + 16].copy_from_slice(&0u32.to_le_bytes());
        let (d, m) = rig.run(&mut s, Stage::Ta, m, &mut stick);
        assert!(!d.pass && d.totals.writes == 0 && m.commands.len() == 26 && d.line.contains("passt nicht"), "{kind}: {}", d.line);
    }
}

#[test]
fn the_smu_is_only_asked_and_a_stopped_one_gets_no_message() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (d, m) = rig.run(&mut s, Stage::Smu, model(), &mut Stick::full());
    assert!(d.pass, "{:?}", d.lines);
    // As Linux leaves it (amdgpu_reg_access.c:590-610): on the address it read, nothing put back.
    assert_eq!(m.index, rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS);
    let f = s.smu.as_ref().unwrap();
    assert_eq!(f.answer(0x02, 0), Some(crate::rdna4::fakemodel::SMU_VERSION));
    assert!(d.lines.iter().any(|l| l.contains("smu version 104.80.0")), "{:?}", d.lines);
    let mut stopped = model();
    stopped.smu_flags = 0;
    let (d, m) = rig.run(&mut Session::default(), Stage::Smu, stopped, &mut Stick::full());
    assert!(!d.pass && m.messages.is_empty() && d.line.contains("läuft nicht"), "{}", d.line);
}

#[test]
fn a_stopped_stage_keeps_the_lines_it_collected() {
    let rig = Rig::new();
    // A bootloader that does not answer the first part, and a secure OS that is ready with a status: both stages
    // collect lines, then stop.
    let (mut s, mut m) = rig.up_to(&[]);
    m.boot.silent_at = Some(0);
    let (d1, _) = rig.run(&mut s, Stage::Sos, m, &mut Stick::full());
    let (mut s, mut m) = rig.up_to(&[Stage::Sos]);
    m.c64 = 0x80C0_0007;
    let (d2, _) = rig.run(&mut s, Stage::Ring, m, &mut Stick::full());
    for (d, first) in [(d1, "firmware buffer: 1 MiB at mc"), (d2, "vram: mc base")] {
        assert!(!d.pass, "{:?}: {:?}", d.stage, d.lines);
        assert!(d.lines.iter().any(|l| l.starts_with(first)), "{:?}: {:?}", d.stage, d.lines);
        assert!(d.lines.last().is_some_and(|l| l.starts_with("stopped: ")), "{:?}: {:?}", d.stage, d.lines);
        assert_eq!(d.lines.iter().filter(|l| l.starts_with("stopped: ")).count(), 1, "{:?}: {:?}", d.stage, d.lines);
        assert!(d.lines.iter().any(|l| l.starts_with("WAIT ")), "the waits that ran out are named: {:?}", d.lines);
    }
}

#[test]
fn the_guard_refuses_by_construction_whatever_a_stage_lists() {
    use crate::gpucore::psp::Hw;
    use crate::rdna4::guard::{Allow, Guard};
    let m = model();
    let r = m.regs.clone();
    let sp = |name| r.named.iter().find(|(_, x)| x.name == name).unwrap().0;
    let listed = std::vec![sp("MPASP_SMN_C2PMSG_35"), sp("MPASP_SMN_C2PMSG_115"), sp("MPASP_SMN_C2PMSG_101"), sp("MP1_SMN_C2PMSG_75"), r.psp.c2pmsg_64, r.smu_message];
    let mut g = Guard::new(m, Allow { stage: "test", regs: listed.clone(), mem: Vec::new() }, r.clone());
    for b in &listed[..4] {
        assert!(!g.reg_write(*b, 0x2_0000));
    }
    assert!(!g.reg_write(r.psp.c2pmsg_64, rdna4_core::refuse::GFX_CTRL_MODE1_RESET));
    assert!(!g.reg_write(r.smu_message, 0x32), "SetPptLimit");
    assert!(!g.reg_write(r.smu_message, 0x28), "AllowGfxOff, not reviewed");
    assert!(!g.reg_write(r.psp.c2pmsg_69, 0), "not listed");
    assert!(!g.mem_write(MC_BASE, &[0; 4]), "no memory listed");
    assert!(!g.doorbell_write64(0, 0));
    assert_eq!(g.refused.len(), 10);
    assert!(g.hw.writes.is_empty(), "nothing reached the card");
    assert!(matches!(g.refused[0], Refusal::Reg { .. }));
}

#[test]
fn the_firmware_list_is_linuxs_order_and_types() {
    let f = files();
    let refs: Vec<&[u8]> = f.iter().map(Vec::as_slice).collect();
    let list = fwlist::list(&refs).unwrap();
    let got: Vec<(u32, Vec<u8>)> = list.iter().map(|u| (u.id.fw_type(), f[u.file][u.range.clone()].to_vec())).collect();
    assert_eq!(got, linux_images());
    // The SMU's firmware first, RLC_G the last before the autoload, then VCN and DMUB; 22 images, 20 before.
    assert_eq!((list.len(), fwlist::before_autoload(&list), BEFORE_AUTOLOAD), (22, 20, 20));
    assert_eq!((list[0].id, list[19].id, list[20].id, list[21].id), (Id::Smc, Id::RlcG, Id::Vcn, Id::Dmcub));
    // The real RLC file's save/restore control list is 0 bytes: no such image, as Linux skips it.
    assert!(list.iter().all(|u| u.id != Id::RlcRestoreListCntl));
    // The secure processor's own two files.
    let s = SosFile::parse(&sos()).unwrap();
    assert_eq!((s.toc().unwrap().range.len(), s.sos_version(), s.parts.len()), (2304, Some(0x3A_1214), 11));
    assert_eq!(s.part(pspfiles::TYPE_RL).map(|d| d.range.len()), Some(107));
    for ((part, _), (_, bytes)) in rdna4_core::bootseq::SEQUENCE.iter().zip(linux_boot()) {
        assert_eq!(sos()[s.boot(*part).unwrap().range.clone()], bytes[..], "{part:?}");
    }
    let t = TaFile::parse(&ta()).unwrap();
    assert_eq!([pspfiles::TA_ASD, pspfiles::TA_RAS, pspfiles::TA_HDCP, pspfiles::TA_DTM].map(|k| t.app(k).map(|d| d.range.len())), [Some(15474), Some(788), Some(1576), Some(356)]);
    assert!(t.app(2).is_none(), "no XGMI application in this file");
    // A header version Linux does not read, a part past the file, a type twice, too many descriptors: refused.
    let mut bad = f.clone();
    bad[0][8] = 2;
    let refs: Vec<&[u8]> = bad.iter().map(Vec::as_slice).collect();
    assert!(fwlist::list(&refs).is_err());
    assert!(fwlist::list(&refs[..9]).is_err(), "a file short");
    let with = |change: &dyn Fn(&mut Vec<u8>)| {
        let mut s = sos();
        change(&mut s);
        SosFile::parse(&s)
    };
    assert!(with(&|s| s[36 + 6 * 16 + 12..36 + 6 * 16 + 16].copy_from_slice(&0x10_0000u32.to_le_bytes())).is_err());
    assert!(with(&|s| s[36 + 16..36 + 20].copy_from_slice(&3u32.to_le_bytes())).is_err());
    assert!(with(&|s| s[32..36].copy_from_slice(&26u32.to_le_bytes())).is_err());
    assert!(with(&|s| s[32..36].copy_from_slice(&14u32.to_le_bytes())).is_err(), "descriptors that would run into the array");
    assert!(with(&|s| s[10] = 1).is_err(), "v2.1 is another layout");
    assert!(with(&|_| {}).is_ok());
    assert_eq!(fwlist::name_for("amdgpu/gc_12_0_1_rlc.bin", 0x7550, 0xC0), "amdgpu/gc_12_0_1_rlc.bin");
    assert_eq!(fwlist::name_for(fwlist::SOS, 0x7551, 0xC8), "amdgpu/psp_14_0_3_sos_kicker.bin");
}

#[test]
fn every_plain_line_is_german_without_code_words() {
    let rig = Rig::new();
    let mut lines = Vec::new();
    for st in Stage::ALL {
        let (d, _) = rig.run(&mut Session::default(), st, model(), &mut Stick::full());
        lines.push(d.line);
    }
    // Each part of the secure OS's load that the card does not take, by its own words.
    for i in 0..9 {
        let (mut s, mut m) = rig.up_to(&[]);
        m.boot.silent_at = Some(i);
        let (d, m) = rig.run(&mut s, Stage::Sos, m, &mut Stick::full());
        assert!(!d.pass && lines.iter().all(|l| *l != d.line), "{}", d.line);
        lines.push(d.line);
        lines.push(rig.run(&mut s, Stage::Sos, m, &mut Stick::full()).0.line);
    }
    let (mut s, m) = rig.up_to(&ORDER);
    lines.extend(rig.all(&mut s, m, &mut Stick::full(), &Stage::ALL).0.into_iter().map(|d| d.line));
    for l in &lines {
        assert!(l.starts_with("Grafikkarte 1 (zeigt das Bild): "), "{l}");
        assert!(!l.contains('_') && !l.contains("0x") && !l.contains("PSP") && !l.contains("SMU") && !l.contains("TA") && !l.contains("SOS"), "{l}");
    }
}

/// The stop note of a card goes into a slot keyed by its address: it never overwrites the 780M's.
#[test]
fn the_note_is_keyed_by_the_pci_address() {
    use crate::devs::Key;
    use crate::gpucore::handoff::{GcFate, NoteIo, RingFate, StopNote, TAG, tag_for};
    use std::sync::Mutex;
    static SLOTS: Mutex<Vec<(u32, Vec<u8>)>> = Mutex::new(Vec::new());
    fn put(t: u32, b: &[u8]) -> bool {
        let mut s = SLOTS.lock().unwrap();
        s.retain(|(x, _)| *x != t);
        s.push((t, b.to_vec()));
        true
    }
    fn take(t: u32) -> Option<Vec<u8>> {
        SLOTS.lock().unwrap().iter().find(|(x, _)| *x == t).map(|(_, b)| b.clone())
    }
    let io = NoteIo { put, take, from_reload: || true };
    let apu = StopNote { ring: RingFate::NotDestroyed, gc: GcFate::Booted, tmr_destroyed: false, bus_master_left_on: true, wptr: 0x10, size: 0x1000, addr_lo: 1, addr_hi: 2 };
    let card = StopNote { ring: RingFate::Destroyed, gc: GcFate::Untouched, tmr_destroyed: false, bus_master_left_on: false, wptr: 0x20, size: 0x1000, addr_lo: 3, addr_hi: 4 };
    let (a, b) = (Key::new(0, 4, 0, 0), Key::new(0, 7, 0, 0));
    assert!(io.put_note(&apu) && io.put_note_for(a, &card));
    assert_eq!(io.take_note(), Some(apu), "the 780M's note stays");
    assert_eq!(io.take_note_for(a), Some(card));
    assert_eq!(io.take_note_for(b), None);
    let tags: Vec<u32> = [a, b, Key::new(0, 0xc7, 0, 0), Key::new(0, 4, 0, 1)].iter().map(|k| tag_for(*k).unwrap()).collect();
    assert!(tags.iter().all(|t| *t != TAG));
    for (i, t) in tags.iter().enumerate() {
        assert!(tags[i + 1..].iter().all(|u| u != t));
    }
    assert_eq!(tag_for(Key::new(1, 4, 0, 0)), None);
}

#[test]
fn the_stage_arguments() {
    use crate::rdna4::livestage::parse;
    let p = |t: &str| parse(&mcp::Json::parse(t).unwrap());
    for st in Stage::ALL {
        assert_eq!(p(&std::format!(r#"{{"stage":"{}"}}"#, st.name())).map(|o| o.stage), Ok(st));
        assert_eq!(Stage::parse(st.name()), Some(st));
    }
    // Linux's chain first, in Linux's order; then the looks and the ring's destroy.
    assert_eq!(Stage::ALL.map(Stage::name), ["nbif", "gmc", "ih", "sos", "ring", "toc", "tmr", "fw", "autoload", "fwrest", "ta", "pmfw", "gcboot", "gchub", "gccp", "mes", "mesq", "gcring", "sdma", "sdmatest", "ringtest", "alive", "bootloader", "smu", "destroy", "state"]);
    assert_eq!(Stage::ALL[..Stage::CHAIN.len()], Stage::CHAIN);
    assert!(p(r#"{"stage":"setup_tmr"}"#).unwrap_err().ends_with("one of nbif, gmc, ih, sos, ring, toc, tmr, fw, autoload, fwrest, ta, pmfw, gcboot, gchub, gccp, mes, mesq, gcring, sdma, sdmatest, ringtest, alive, bootloader, smu, destroy, state"));
    assert!(p(r#"{"until":"ring"}"#).is_err(), "a card takes stage, not until");
}
