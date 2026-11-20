//! Wave 42 as a whole: Linux's chain from a cold card to loaded firmware (`Stage::CHAIN`: nbif, gmc, ih, then the
//! secure processor's sos, ring, toc, tmr, fw, autoload, fwrest, ta; since wave 43 the copy engines' sdma behind
//! them) on the model that starts cold in every respect. One run of it equals every section of Linux's trace this wave covers, in one go (`chainseq`); each
//! stage refuses, without an access, until the one before it passed, across the blocks too; the looks outside the
//! chain change nothing of it; and the VRAM manager's reservations hold across both halves (the picture, the
//! secure processor's window, the GART table, the default page, the firmware's own region), on the driver's side
//! and on the model's.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::chainseq::{self, Chosen};
use crate::rdna4::gcstage;
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::{MC_BASE, Model, STAGE_STACK, stack_at};
use crate::rdna4::guard::{Guard, Refusal};
use crate::rdna4::nbregs::NbRegs;
use crate::rdna4::stage::{self, Done, Session, Sos, Stage};
use crate::rdna4::vram::Kept;
use crate::rdna4::{linuxseq, nb};
use core::ops::Range;
use rdna4_core::ih7::{Iv, Ring};
use std::vec::Vec;

const CHAIN: [Stage; 19] = Stage::CHAIN;

/// The chain on a cold model, nothing before it: every stage's answer, with every write the model saw.
fn chain() -> (Rig, Session, Vec<Done>, Model) {
    let rig = Rig::new();
    let mut s = Session::default();
    // `powered`: the power controller's stage is not in this chain yet; its one effect on the model stands in.
    let (done, m) = rig.pass(&mut s, model(), &mut Stick::full(), &CHAIN);
    (rig, s, done, m)
}

fn overlap(a: &Range<u64>, b: &Range<u64>) -> bool {
    a.start < b.end && b.start < a.end
}

#[test]
fn the_whole_chain_equals_every_covered_section_of_linuxs_trace_in_one_go() {
    let (rig, s, done, m) = chain();
    let l = s.layout.clone().unwrap();
    let compared = chainseq::equal(&done, &regs(&rig), &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_or_else(|e| panic!("{e}"));
    // Linux's order of sections, each whole: (name, Linux's lines). init.txt lines 9-16, 17-205, 624-670, 673-751,
    // 752-892, 893-1883, 9151-9191, 9192-9361, 9502-9567 and 9604-9671; a line is a write, a read, a wait, a sleep
    // or a flush.
    let lines: Vec<(&str, usize)> = compared.iter().map(|(n, lines, _)| (*n, *lines)).collect();
    assert_eq!(lines, [("nbif", 8), ("mmhub", 175), ("ih", 47), ("psp-boot", 47), ("psp-ring", 100), ("smu", 741), ("gc-boot", 41), ("gc-hub", 168), ("gc-cp", 135), ("mes", 64), ("gc-ring", 26), ("sdma", 68)]);
    assert!(compared.iter().all(|(_, _, accesses)| *accesses > 0));
    // The fixtures the kernel carries are these and the flush after a bind; `discovery` is not among them.
    let carried: Vec<&str> = linuxseq::SECTIONS.iter().map(|(n, _)| *n).collect();
    assert_eq!(carried, ["nbif", "mmhub", "sw-init", "ih", "psp-boot", "psp-ring", "smu", "gc-boot", "gc-hub", "gc-cp", "mes", "gc-ring", "sdma"]);
    assert_eq!(chainseq::COVER.map(|(n, _)| n), ["nbif", "mmhub", "ih", "psp-boot", "psp-ring", "smu", "gc-boot", "gc-hub", "gc-cp", "mes", "gc-ring", "sdma"]);
    assert_eq!(chainseq::COVER.iter().flat_map(|(_, st)| st.iter().copied()).collect::<Vec<_>>(), CHAIN);
    // Nothing was refused on either side, and the card got all of it.
    assert!(m.refused.is_empty() && done.iter().all(|d| d.refused.is_empty()), "{:?}", m.refused);
    assert_eq!((m.boot.taken, m.commands.len(), m.next()), (9, 31, None));
    assert_eq!((s.sos, s.next()), (Some(Sos::Loaded { sol: 0x0E5B_5A34 }), None));
    assert!(CHAIN.iter().all(|st| s.passed(*st)) && s.autoload && s.ta_done);
    assert!(m.nb.started() && m.nb.remapped == [true, true] && m.nb.context0);
    // Every HDP flush of the chain went to Linux's one place: gmc 2, ih's bind 2, nine parts, 31 commands,
    // gchub 2, MES's bind with both hubs on 3 (the bind's own, GFXHUB's, MMHUB's), the kernel queues' two, and
    // the bind of the copy engines' rings 3 again, now with the hubs' invalidations through MES.
    assert_eq!(m.flushes, 2 + 2 + 9 + 31 + 2 + 3 + 2 + 3);
    // The graphics block: every engine's start on every pipe, one reset pulse each, pipe 0 selected again, all
    // three still halted, and both hubs on the one table.
    assert_eq!((m.gc.starts.len(), m.gc.pulses, m.gc.grbm), (2 * (2 + 2 + 4), [1, 1, 1], 0));
    assert!(m.gc.context0 && s.gc.hub && s.gc.boot.is_some_and(|b| b.rounds == 1 && b.gb_addr_config == 0x0820_0545));
    // GFXHUB was flushed at its start, with MES's bind by the CPU and with the engines' bind by MES.
    assert_eq!(m.gc.tlb_flushes, 3);
    // MES runs on both pipes, took Linux's commands and mapped its own queue and the kernel's five, handing those
    // to the engines' stand-in; the engines' bind went out as two of its WR_WAIT_WR_REG commands.
    assert!(m.mes.running == [true, true] && m.mes.hw.iter().all(Option::is_some) && m.mes.hw1 == [1, 1] && m.mes.change_config == 1);
    assert_eq!((m.mes.mapped.len(), m.cp.queues.len(), m.mes.misc_writes.len()), (6, 5, 2));
    assert!(s.mes.ready() && s.mes.kq);
    assert!(m.writes.iter().all(|(b, _)| *b != m.regs.hdp_direct));
}

/// A stage runs on MCP's 64 KiB thread on the real machine: none of the chain holds anything large on the stack
/// when it reaches the card (`fakemodel::STAGE_STACK`; the kernel's own frames are held to it by `gx.rdna4.stages`).
#[test]
fn no_stage_of_the_chain_is_deep_in_the_stack_when_it_reaches_the_card() {
    let rig = Rig::new();
    let (mut s, mut m, mut stick) = (Session::default(), model(), Stick::full());
    // An unoptimised host build's frames are larger than the kernel's: 20 KiB measured at the deepest (`ring`).
    let limit = STAGE_STACK * 3 / 2;
    let top = stack_at();
    for st in CHAIN {
        m.deepest = usize::MAX;
        (_, m) = rig.pass(&mut s, m, &mut stick, &[st]);
        let deep = top.saturating_sub(m.deepest);
        assert!(deep <= limit, "{st:?} reached the card {deep} bytes deep in the stack, over {limit}: what is large lies on the heap");
    }
}

/// A difference anywhere in the chain is named with its section and stage: the comparison in one go is no rubber
/// stamp.
#[test]
fn a_chain_that_is_not_linuxs_is_named() {
    let (rig, s, done, _) = chain();
    let (r, l) = (regs(&rig), s.layout.clone().unwrap());
    // Out of Linux's order, or short.
    let mut swapped = done.clone();
    swapped.swap(1, 2);
    assert!(chainseq::equal(&swapped, &r, &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_err().starts_with("not Linux's chain"));
    assert!(chainseq::equal(&done[..10], &r, &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_err().starts_with("not Linux's chain"));
    // One access taken out of a stage, in every section.
    for (at, section) in [(0, "nbif"), (1, "mmhub"), (2, "ih"), (3, "psp-boot"), (5, "psp-ring"), (11, "smu"), (12, "gc-boot"), (13, "gc-hub"), (14, "gc-cp"), (15, "mes"), (16, "mes"), (17, "gc-ring"), (18, "sdma")] {
        let mut v = done.clone();
        let middle = v[at].trail.len() / 2;
        v[at].trail.remove(middle);
        let e = chainseq::equal(&v, &r, &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_err();
        assert!(e.starts_with(section), "{section}: {e}");
    }
    // A trail that lost accesses proves nothing.
    let mut v = done.clone();
    v[4].trail_lost = 1;
    assert!(chainseq::equal(&v, &r, &l, s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_err().contains("1 accesses lost"));
}

fn said(d: &Done) -> Vec<&str> {
    d.lines.iter().map(|l| l.as_str()).filter(|l| l.starts_with("linuxseq ")).collect()
}

/// Every stage call says itself how its accesses stand against Linux's trace, in the last technical line of its
/// answer: the comparison the real card's answers carry too, where no test stands by. A section of several
/// stages is equal so far until its last stage ends it.
#[test]
fn every_stage_of_the_chain_says_how_it_stands_against_linuxs_trace() {
    let (rig, mut s, done, m) = chain();
    let mut goes_on = Vec::new();
    for (d, want) in done.iter().zip(["nbif", "mmhub", "ih", "psp-boot", "psp-ring", "psp-ring", "psp-ring", "psp-ring", "psp-ring", "psp-ring", "psp-ring", "smu", "gc-boot", "gc-hub", "gc-cp", "mes", "mes", "gc-ring", "sdma"]) {
        let line = d.lines.last().unwrap().as_str();
        assert_eq!(said(d), [line], "{:?}: one line, the last", d.stage);
        if d.stage == Stage::Mes {
            // MES's start is the section's first stage: the kernel queues' mapping (`mesq`) ends it.
            assert_eq!(line, "linuxseq mes: equal so far (Linux goes on at init.txt line 9554)");
            continue;
        }
        let whole = d.stage == Stage::Ta || want != "psp-ring";
        if whole {
            assert_eq!(line, std::format!("linuxseq {want}: equal"), "{:?}", d.stage);
        } else {
            let at = line.strip_prefix("linuxseq psp-ring: equal so far (Linux goes on at init.txt line ").and_then(|r| r.strip_suffix(')')).unwrap_or_else(|| panic!("{:?}: {line}", d.stage));
            goes_on.push(at.parse::<u32>().unwrap());
        }
    }
    // ring, toc, tmr, fw, autoload, fwrest: each goes further into the section, but tmr, which sends nothing.
    assert_eq!(goes_on.len(), 6);
    assert!(goes_on[0] > 752 && goes_on[1] == goes_on[2] && goes_on.windows(2).all(|w| w[0] <= w[1]) && goes_on[5] < 892, "{goes_on:?}");
    assert!(goes_on[0] < goes_on[1] && goes_on[2] < goes_on[3] && goes_on[3] < goes_on[4] && goes_on[4] < goes_on[5], "{goes_on:?}");
    // The looks and the destroy stand in no section, and a stage refused before any access said nothing.
    let (looks, m) = rig.all(&mut s, m, &mut Stick::full(), &[Stage::Alive, Stage::Bootloader, Stage::State, Stage::Ta, Stage::Destroy]);
    assert!(looks.iter().all(|d| said(d).is_empty()), "{:?}", looks.iter().map(said).collect::<Vec<_>>());
    // A second ring after the destroy begins its section anew, and it is not Linux's start: on a secure OS that
    // had a ring the stage looks at the ring's registers first, which the line names from the section's first line.
    let (again, _) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Ring]);
    let line = said(&again[0])[0];
    assert!(line.starts_with("linuxseq psp-ring: differs: psp-ring.txt line 4 (init.txt line 752): Linux waits on MPASP_SMN_C2PMSG_64") && line.contains("the stage reads MPASP_SMN_C2PMSG_67"), "{line}");
}

/// The line is no rubber stamp, and it tells the stage's doing from the card's: an access that is not Linux's is
/// the first difference; an answer of the card that is not the trace's is counted and the accesses after it are
/// compared on; a call that stopped is compared as far as it came and counted before the next.
#[test]
fn the_line_names_a_difference_an_answer_of_the_cards_and_a_call_that_stopped() {
    let (rig, s, done, _) = chain();
    let (r, l) = (regs(&rig), s.layout.clone().unwrap());
    let vram = rig.att.vram_bytes;
    let told = s.nb.gart.as_ref().map(gcstage::Told::of);
    let ask = |seen: &mut chainseq::Seen, d: &Done, trail: &[Access], lost| chainseq::verdict(seen, d.stage, d.pass, trail, lost, &r, vram, Chosen { l: Some(&l), taken: s.bump.as_ref(), told: told.as_ref(), irq: &[], mes: s.mes.kiq, sdma: s.sdma.bound }).unwrap();
    // One access taken out, in each half; a trail that lost accesses proves nothing.
    for (at, section) in [(0, "nbif"), (1, "mmhub"), (2, "ih"), (3, "psp-boot"), (4, "psp-ring")] {
        let mut t = done[at].trail.clone();
        t.remove(t.len() / 2);
        let line = ask(&mut chainseq::Seen::new(), &done[at], &t, 0);
        assert!(line.starts_with(&std::format!("linuxseq {section}: differs: {section}")), "{line}");
        assert!(ask(&mut chainseq::Seen::new(), &done[at], &done[at].trail, 3).contains("differs") && ask(&mut chainseq::Seen::new(), &done[at], &done[at].trail, 3).contains("lost 3 accesses"));
    }
    // The section's later stages are held from its start: a difference in `ring` shows in every line after it,
    // and a stage with nothing on record before it is not compared at all.
    let mut seen = chainseq::Seen::new();
    let mut short = done[4].trail.clone();
    short.remove(short.len() / 2);
    assert!(ask(&mut seen, &done[4], &short, 0).starts_with("linuxseq psp-ring: differs: "));
    assert!(ask(&mut seen, &done[5], &done[5].trail, 0).starts_with("linuxseq psp-ring: differs: "));
    assert_eq!(ask(&mut chainseq::Seen::new(), &done[5], &done[5].trail, 0), "linuxseq psp-ring: not compared: what the section's stages before toc did is not on record in this boot");
    // Before `gmc` planned the window the secure processor's sections have no rules.
    assert!(chainseq::verdict(&mut chainseq::Seen::new(), Stage::Sos, true, &done[3].trail, 0, &r, vram, Chosen { l: None, taken: s.bump.as_ref(), ..Chosen::default() }).unwrap().starts_with("linuxseq psp-boot: not compared: no window is planned"));
    // The card's own doing: the semaphore of gmc's flush never comes, Linux logs the wait and goes on, so does
    // the stage. The accesses are Linux's, one answer is not the trace's.
    let (mut s, mut m) = (Session::default(), model());
    m.nb.sem_never = true;
    let (d, mut m) = rig.all(&mut s, m, &mut Stick::full(), &[Stage::Nbif, Stage::Gmc]);
    assert!(d[1].pass, "{:?}", d[1].lines);
    let line = said(&d[1])[0];
    assert!(line.starts_with("linuxseq mmhub: equal; 1 answers of the card are not the trace's, the first: mmhub.txt line ") && line.ends_with("the card does not answer as it answered Linux"), "{line}");
    // A call that stopped after its accesses: `ih` on a machine with no memory for the rings has asked nothing
    // of the card yet and says nothing; `gmc` that may not hand out system pages stops after its four reads.
    m.nb.pages_refused = true;
    let (d, _) = rig.all(&mut s, m, &mut Stick::full(), &[Stage::Ih]);
    assert!(!d[0].pass && d[0].trail.is_empty() && said(&d[0]).is_empty(), "{:?}", d[0].lines);
    let (mut s, mut m) = (Session::default(), model());
    m.nb.survey = Err("TranslationOn");
    let (d, mut m) = rig.all(&mut s, m, &mut Stick::full(), &[Stage::Nbif, Stage::Gmc]);
    let reads = d[1].trail.len();
    assert!(!d[1].pass && reads == 4 && d[1].lines.last().unwrap().starts_with("stopped: "), "{:?}", d[1].lines);
    assert!(said(&d[1])[0].starts_with("linuxseq mmhub: the same as Linux's as far as the stage came (Linux goes on at init.txt line "), "{:?}", said(&d[1]));
    m.nb.survey = Ok(());
    let (d, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Gmc, Stage::Ih]);
    assert_eq!(said(&d[0]), ["linuxseq mmhub: equal; 4 accesses of calls that stopped came before it and are not compared"]);
    assert_eq!(said(&d[1]), ["linuxseq ih: equal"]);
    assert!(m.refused.is_empty());
}

#[test]
fn each_stage_refuses_without_an_access_until_the_one_before_it_passed() {
    let rig = Rig::new();
    for k in 1..CHAIN.len() {
        // Everything before CHAIN[k - 1] passed; CHAIN[k - 1] itself did not run.
        let mut s = Session::default();
        let (_, mut m) = rig.pass(&mut s, model(), &mut Stick::full(), &CHAIN[..k - 1]);
        assert_eq!((s.next(), s.before(CHAIN[k])), (Some(CHAIN[k - 1]), Some((CHAIN[k - 1], false))));
        let (writes, host, sys) = (m.writes.len(), m.nb.host_changes.len(), m.nb.sys_given.len());
        for st in &CHAIN[k..] {
            let (d, back) = rig.run(&mut s, *st, m, &mut Stick::full());
            m = back;
            assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && d.refused.is_empty(), "without {:?}: {st:?} {}", CHAIN[k - 1], d.line);
        }
        assert!(m.writes.len() == writes && m.nb.host_changes.len() == host && m.nb.sys_given.len() == sys && m.refused.is_empty(), "without {:?}", CHAIN[k - 1]);
        // The stage that was left out then runs, and the one after it too.
        let (_, _) = rig.pass(&mut s, m, &mut Stick::full(), &CHAIN[k - 1..=k]);
    }
    // The chain's first has nothing before it; the looks and the destroy stand outside.
    let s = Session::default();
    assert_eq!(s.before(Stage::Nbif), None);
    for look in [Stage::Alive, Stage::Bootloader, Stage::Smu, Stage::State, Stage::Destroy] {
        assert_eq!((s.before(look), s.passed(look)), (None, false));
    }
}

/// The looks need no place in the chain and take none: with all of them asked between every two stages the chain's
/// own accesses are still Linux's.
#[test]
fn the_looks_between_the_stages_change_nothing_of_the_chain() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut m = model();
    let mut stick = Stick::full();
    let mut done = Vec::new();
    for st in CHAIN {
        // The SMU is asked on the cold card and again right before its own start (the firmware `fw` loaded is a
        // new one: the look's first message there waits for no answer, as Linux's first does not).
        let looks: &[Stage] = if matches!(st, Stage::Nbif | Stage::Pmfw) { &[Stage::Alive, Stage::Bootloader, Stage::State, Stage::Smu] } else { &[Stage::Alive, Stage::Bootloader, Stage::State] };
        let (seen, back) = rig.pass(&mut s, m, &mut stick, looks);
        assert!(seen.iter().take(3).all(|d| d.totals.writes == 0));
        let (d, back) = rig.pass(&mut s, back, &mut stick, &[st]);
        m = back;
        done.extend(d);
    }
    chainseq::equal(&done, &regs(&rig), &s.layout.clone().unwrap(), s.bump.as_ref(), s.mes.kiq, s.sdma.bound).unwrap_or_else(|e| panic!("{e}"));
    assert!(m.refused.is_empty() && m.next().is_none());
}

#[test]
fn the_reservations_hold_across_both_halves() {
    let (rig, mut s, _, m) = chain();
    let l = s.layout.clone().unwrap();
    let bump = s.bump.as_ref().unwrap();
    let g = s.nb.gart.as_ref().unwrap();
    // One manager: the picture, the window and the firmware's two regions are kept; the GART table and the default
    // page were taken outside them; the secure processor's buffers all came out of the window.
    let kept: Vec<Kept> = bump.kept().iter().map(|(k, _)| *k).collect();
    assert_eq!(kept, [Kept::Picture, Kept::PspWindow, Kept::Firmware, Kept::Firmware]);
    // The power controller's two tables follow them, lowest first: the tools table, then the driver table.
    let (tools, driver) = s.pmfw.tables.clone().unwrap();
    assert_eq!((tools.end - tools.start, driver.end - driver.start, tools.end), (0x19000, 0x4000, driver.start));
    // The clear-state buffer follows the driver table, as Linux's did (MC 0x83_daafc000 right behind 0x83_daaf8000),
    // and MES's two `resource_1` pages follow it.
    let csb = s.cp.csb.clone().unwrap();
    assert_eq!(csb, driver.end..driver.end + 0x1000);
    let r1 = s.mes.resource1.clone().unwrap();
    assert_eq!((r1.end - r1.start, r1.start), (2 * 4096, csb.end));
    assert_eq!(
        bump.taken.iter().map(|t| (t.0, t.1.clone())).collect::<Vec<_>>(),
        [("GART table", g.table.clone()), ("default page", g.scratch.clone()), ("the PMFW's tools table", tools.clone()), ("the PMFW's driver table", driver.clone()), ("the clear-state buffer", csb.clone()), ("MES resource_1 pages", r1.clone())]
    );
    assert!(!bump.given.is_empty() && bump.given.iter().all(|r| r.start >= l.window.start && r.end <= l.window.end));
    let fw: Vec<Range<u64>> = s.reserved.unwrap().iter().map(|r| r.base..r.base + u64::from(r.size)).collect();
    assert_eq!((fw[0].end, fw[0].end - fw[0].start, fw[1].end - fw[1].start), (16304 << 20, 50 << 20, 4 << 20));
    let mut all: Vec<(&str, Range<u64>)> = std::vec![("picture", 0..PICTURE), ("window", l.window.clone()), ("GART table", g.table.clone()), ("default page", g.scratch.clone()), ("firmware", fw[0].clone()), ("firmware, extended", fw[1].clone()), ("tools table", tools), ("driver table", driver), ("clear-state buffer", csb), ("MES resource_1", r1)];
    all.extend(bump.given.iter().skip(1).map(|r| ("a buffer of the secure processor", r.clone())));
    for (i, a) in all.iter().enumerate() {
        for b in &all[i + 1..] {
            let inside_window = a.0 == "window" && b.0 == "a buffer of the secure processor";
            assert!(inside_window || !overlap(&a.1, &b.1), "{} {:x?} lies over {} {:x?}", a.0, a.1, b.0, b.1);
        }
    }
    // What the secure processor was told lies in the window: the firmware buffer and every image.
    let in_window = |mc: u64, bytes: u64| mc >= l.window_mc().start && mc + bytes <= l.window_mc().end;
    assert!(in_window(s.pri_mc().unwrap(), 1 << 20) && s.loaded.iter().all(|x| in_window(x.1, u64::from(x.2))) && s.loaded.len() == 22);
    // The model holds the same: what GMC named to the hub is the manager's two ranges.
    assert_eq!(m.nb.vram_held(), [("the GART table", g.table.clone()), ("the default page", g.scratch.clone())]);
    // The GART's address space holds the IH's one bind, as `ih` left it, and behind it MES's one (with the kernel
    // queues' memory) and the copy engines' one.
    let ih = s.nb.ih.as_ref().unwrap();
    assert_eq!(g.bound.iter().map(|b| (b.0, b.1, b.2.len())).collect::<Vec<_>>(), [("IH rings", 1, 129), ("MES rings, MQDs and the kernel queues", 130, 38), ("SDMA rings", 168, 6)]);
    // Each half's write list reaches its own memory and not the other's.
    let (r, n) = (regs(&rig), NbRegs::from(&rig.att.discovery, &regs(&rig)).unwrap());
    let ih_list = nb::allow(Stage::Ih, &s.nb, &n, |x| l.mc(x.start)..l.mc(x.end));
    let fw_list = stage::allow(Stage::Fw, &r, Some(&l));
    assert_eq!((ih_list.mem.clone(), fw_list.mem.clone()), (std::vec![g.table_mc()], std::vec![l.window_mc()]));
    let (table_mc, pri_mc) = (g.table_mc().start, s.pri_mc().unwrap());
    let writes = m.writes.len();
    let mut guard = Guard::new(m, fw_list, r.clone());
    assert!(!guard.mem_write(table_mc + 8, &[0xFF; 8]), "a stage of the secure processor wrote into the GART table");
    assert_eq!(guard.refused, [Refusal::Mem { mc: table_mc + 8, bytes: 8 }]);
    let mut guard = Guard::new(guard.hw, ih_list, r.clone());
    assert!(!guard.mem_write(pri_mc, &[0xFF; 8]), "the ih stage wrote into the secure processor's window");
    let mut m = guard.hw;
    assert_eq!(m.writes.len(), writes);
    // After all 31 commands the GART table is whole: an interrupt still finds the ring through it.
    let sys = ih.place.sys;
    m.raise(Ring::Ring0, &Iv { client_id: 4, src_id: 9, src_data: [6, 0, 0, 0], ..Iv::default() }).unwrap();
    let reader = &mut s.nb.ih.as_mut().unwrap().reader;
    assert_eq!(reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, 1);
    // The ring's destroy gives the window back whole; the table, the default page and the firmware's regions stay,
    // and so do the GART and the IH. A second ring finds the same places.
    let table = s.nb.gart.as_ref().unwrap().table.clone();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy]);
    let bump = s.bump.as_ref().unwrap();
    // The power controller's two tables stay too: the PMFW keeps their addresses whatever becomes of the ring; so
    // do the clear-state buffer, which the RLC keeps, and MES's two pages, which it writes while it runs.
    assert!(bump.given.is_empty() && bump.taken.len() == 6 && bump.kept().len() == 4 && s.layout.is_some());
    assert!(s.nb.nbif && s.nb.gart.is_some() && s.nb.ih.is_some() && s.next() == Some(Stage::Ring));
    assert!(s.pmfw.passed && s.pmfw.gc_powered() && s.pmfw.tables.is_some(), "the PMFW runs on: its start stands until fw loads its firmware again");
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Ring]);
    assert_eq!((s.nb.gart.as_ref().unwrap().table.clone(), s.given().len(), s.given()[0].start), (table, 4, l.window.start));
    assert!(m.refused.is_empty());
}

/// The secure OS names two regions of its own. A first one that is free and a second one over the GART table: the
/// ring stage stops, its ring goes again, and neither region is kept, so nothing is handed out above half of VRAM
/// on the word of a stage that did not pass.
#[test]
fn a_second_firmware_region_over_the_gart_table_keeps_neither() {
    let rig = Rig::new();
    let (mut s, mut m) = rig.up_to(&[Stage::Sos]);
    let table = s.nb.gart.as_ref().unwrap().table.clone();
    let taken = s.bump.as_ref().unwrap().taken.clone();
    m.reserved_ext_at = Some(table.start);
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("gehört der Karte selbst"), "{}", d.line);
    assert!(d.lines.iter().any(|l| l.contains("GART table")), "the line names what the region lies over: {:?}", d.lines);
    assert_eq!((s.reserved, s.ring_up(), s.next(), m.ring), (None, false, Some(Stage::Ring), None));
    let bump = s.bump.as_mut().unwrap();
    assert!(bump.kept().iter().all(|(k, _)| *k != Kept::Firmware), "{:x?}", bump.kept());
    assert_eq!(bump.taken, taken);
    // The ceiling is still half of VRAM: the free space behind the default page ends there.
    let half = (16304u64 << 20) / 2;
    let room = half - taken.last().unwrap().1.end;
    assert!(bump.take("up to the half", room, 4096).is_ok_and(|r| r.end == half));
    assert!(bump.take("above the half", 4096, 4096).is_err());
    assert!(m.refused.is_empty(), "{:?}", m.refused);
}

/// The model on its own account, for a driver whose manager let a buffer of the secure processor into what GMC
/// holds: the bootloader's megabyte and a ring in the GART table are refused.
#[test]
fn the_model_refuses_a_buffer_of_the_secure_processor_in_the_gart_table() {
    let rig = Rig::new();
    // Cold, the three blocks started: the megabyte of the table as the bootloader's buffer.
    let (s, mut m) = rig.up_to(&[]);
    let table = s.nb.gart.as_ref().unwrap().table.clone();
    assert!(table.start.is_multiple_of(1 << 20) && table.end - table.start == 1 << 20);
    let (c35, c36) = (m.regs.psp.c2pmsg_35, m.regs.c2pmsg_36);
    while m.reg_read(c35) != Some(0x8000_0000) {}
    assert!(!m.reg_write(c36, ((MC_BASE + table.start) >> 20) as u32));
    assert!(m.refused.last().unwrap().contains("no free megabyte of VRAM"), "{:?}", m.refused);
    // The megabyte before it (the window's last) is free.
    assert!(m.reg_write(c36, ((MC_BASE + table.start - (1 << 20)) >> 20) as u32));
    // The secure OS loaded: a ring in the table, and in the default page.
    let (s, mut m) = rig.up_to(&[Stage::Sos]);
    let g = s.nb.gart.as_ref().unwrap();
    let p = m.regs.psp;
    for at in [g.table.start + 0x4000, g.scratch.start] {
        let mc = MC_BASE + at;
        assert_eq!(m.reg_read(p.c2pmsg_64), Some(0x80C0_0000));
        assert!(m.reg_write(p.c2pmsg_69, mc as u32) && m.reg_write(p.c2pmsg_70, (mc >> 32) as u32) && m.reg_write(p.c2pmsg_71, 0x1000));
        assert!(!m.reg_write(p.c2pmsg_64, rdna4_core::refuse::GFX_CTRL_INIT_GPCOM_RING));
        assert!(m.refused.last().unwrap().contains("outside the GART table"), "{:?}", m.refused);
    }
    assert!(m.ring.is_none());
}
