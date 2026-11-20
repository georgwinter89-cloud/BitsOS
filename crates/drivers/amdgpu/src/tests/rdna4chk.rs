//! GP-GPU2 after the checkers: the answers Linux checks and the first version did not (a destroy or a ready with a
//! status, a create that timed out after its registers went out, the SMU's pre-poll), the UCLK's clock id, the
//! version gate of the reserved-region questions, the ring's command allow list, and a card whose secure OS nobody
//! loaded (a cold start: nothing behind the load runs).

use super::rdna4fix::*;
use crate::gpucore::fwload::stick_path;
use crate::gpucore::handoff::RingFate;
use crate::rdna4::fakemodel::CLOCKS;
use crate::rdna4::fwlist;
use crate::rdna4::stage::{Session, Stage};
use std::vec::Vec;

fn uclk() -> (u32, u32) {
    let c = CLOCKS.iter().find(|c| c.0 == 2).unwrap();
    (c.1, c.2)
}

/// A kernel after a Reload: a session of its own on the same card, whose secure OS runs by now. It starts the
/// three blocks before the secure processor again, as Linux does at every start, and finds the secure OS alive
/// (no write to the secure processor).
fn reloaded(rig: &Rig, m: crate::rdna4::fakemodel::Model, stick: &mut Stick) -> (Session, crate::rdna4::fakemodel::Model) {
    let mut s = Session::default();
    let (done, m) = rig.cold_pass(&mut s, m, stick, &[Stage::Sos]);
    assert!(m.writes.is_empty() && done[0].totals.writes == 0);
    (s, m)
}

#[test]
fn a_destroy_answered_with_a_status_leaves_the_ring_up() {
    let rig = Rig::new();
    let mut stick = Stick::full();
    let (mut s, mut m) = rig.up_to(&[Stage::Sos, Stage::Ring]);
    m.destroy_status = Some(5);
    let (d, m) = rig.run(&mut s, Stage::Destroy, m, &mut stick);
    // The note stays the ring stage's "not destroyed" (unchanged, so not handed on again).
    assert!(!d.pass && d.line.contains("lehnt den Befehlskanal ab") && s.note.map(|n| n.ring) == Some(RingFate::NotDestroyed), "{} {:?}", d.line, s.note);
    assert!(m.ring.is_some() && s.ring_up(), "the ring is still the PSP's");
    // Neither this boot nor the kernel after a Reload makes a second ring over it.
    let (d2, m) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(!d2.pass && d2.totals.writes == 0 && d2.line.contains("schon angelegt"), "{}", d2.line);
    let mut rig2 = Rig::new();
    rig2.inherited = s.note;
    let (mut s2, m) = reloaded(&rig2, m, &mut stick);
    let n = m.writes.len();
    let (d3, m) = rig2.run(&mut s2, Stage::Ring, m, &mut stick);
    assert!(!d3.pass && m.writes.len() == n && d3.line.contains("erst abbauen"), "{}", d3.line);
    // A ring from before whose destroy answers a status: the same.
    let (d4, mut m2) = rig2.run(&mut s2, Stage::Destroy, m, &mut stick);
    assert!(!d4.pass && d4.note.map(|n| n.ring) == Some(RingFate::NotDestroyed), "{:?}", d4.lines);
    // A clean destroy, then the ring is made again.
    m2.destroy_status = None;
    let (d5, m2) = rig2.run(&mut s2, Stage::Destroy, m2, &mut stick);
    assert!(d5.pass && m2.ring.is_none(), "{:?}", d5.lines);
    let (d6, m2) = rig2.run(&mut s2, Stage::Ring, m2, &mut stick);
    assert!(d6.pass && m2.refused.is_empty() && m2.ring.is_some(), "{:?} {:?}", d6.lines, m2.refused);
}

#[test]
fn a_create_that_timed_out_after_its_registers_counts_as_up() {
    let rig = Rig::new();
    let mut stick = Stick::full();
    let (mut s, mut m) = rig.up_to(&[Stage::Sos]);
    m.create_late = true;
    let (d, mut m) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(!d.pass && d.note.map(|n| n.ring) == Some(RingFate::NotDestroyed), "{} {:?}", d.line, d.note);
    assert!(m.ring.is_some() && !s.ring_up());
    // The note is made from what the stage wrote (Linux reads none of it back): the ring's place, no frame yet.
    let n = d.note.unwrap();
    assert_eq!((n.wptr, n.size, u64::from(n.addr_hi) << 32 | u64::from(n.addr_lo)), (0, 0x1000, m.ring.unwrap().0));
    // The PSP answers late; still no second INIT_GPCOM_RING, in this boot or the next kernel's.
    (m.c64, m.create_late) = (0x80C2_0000, false);
    let n = m.writes.len();
    let (d2, m) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(!d2.pass && m.writes.len() == n && d2.line.contains("halb angelegt"), "{}", d2.line);
    let (d3, m) = rig.run(&mut s, Stage::Toc, m, &mut stick);
    assert!(!d3.pass && m.writes.len() == n, "{}", d3.line);
    let mut rig2 = Rig::new();
    rig2.inherited = d.note;
    let (mut s2, m) = reloaded(&rig2, m, &mut stick);
    let (d4, m) = rig2.run(&mut s2, Stage::Ring, m, &mut stick);
    assert!(!d4.pass && d4.totals.writes == 0 && m.writes.is_empty(), "{}", d4.line);
    // Only destroy clears it; then the ring is made.
    let (d5, m) = rig.run(&mut s, Stage::Destroy, m, &mut stick);
    assert!(d5.pass && m.ring.is_none(), "{:?}", d5.lines);
    let (d6, m) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(d6.pass && m.refused.is_empty(), "{:?} {:?}", d6.lines, m.refused);
}

#[test]
fn a_secure_os_ready_with_a_status_gets_no_ring() {
    let rig = Rig::new();
    let (mut s, mut m) = rig.up_to(&[Stage::Sos]);
    m.c64 = 0x80C0_0007;
    let writes = m.writes.len();
    let t0 = m.card.now_us;
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("lehnt den Befehlskanal ab"), "{}", d.line);
    assert!(m.writes.len() == writes && m.refused.is_empty() && m.ring.is_none() && s.note.is_none(), "{:?} {:?}", m.writes, m.refused);
    // As Linux: its wait never ends on a status, it runs through its 100000 reads.
    assert_eq!(m.card.now_us - t0, 100_000);
    // The window's pages the ring would have had are free again: the next try gets the same ones.
    let given = s.given().len();
    let (d, mut m) = rig.run(&mut s, Stage::Ring, m, &mut Stick::full());
    assert!(!d.pass && s.given().len() == given, "{}", d.line);
    m.c64 = 0x80C0_0000;
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut Stick::full());
    assert!(d.pass && m.refused.is_empty() && s.given().len() == given + 3, "{:?}", d.lines);
}

#[test]
fn the_uclk_questions_name_the_uclk() {
    let rig = Rig::new();
    let mut s = Session::default();
    let (d, m) = rig.run(&mut s, Stage::Smu, model(), &mut Stick::full());
    assert!(d.pass, "{:?}", d.lines);
    let f = s.smu.as_ref().unwrap();
    assert_eq!((f.answer(0x1D, 2 << 16), f.answer(0x1E, 2 << 16)), (Some(uclk().0), Some(uclk().1)));
    assert_eq!((f.answer(0x1D, 0), f.answer(0x1E, 0)), (Some(500), Some(2970)), "GFXCLK");
    assert!(!m.messages.iter().any(|(_, a)| a >> 16 == 1), "no SOCCLK question: {:x?}", m.messages);
}

#[test]
fn a_first_smu_message_without_answer_spends_the_skipped_pre_poll() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut m = model();
    m.smu_silent_on = Some(0x02);
    let (d, mut m) = rig.run(&mut s, Stage::Smu, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("antwortet nicht") && s.smu_asked, "{}", d.line);
    m.smu_silent_on = None;
    let sent = m.messages.len();
    let (d, m) = rig.run(&mut s, Stage::Smu, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("alten Frage"), "{}", d.line);
    assert_eq!((m.messages.len(), d.totals.writes), (sent, 1), "only the index pair's address, no mailbox write");
    // The stage that starts the power controller would write its first message without looking: it refuses.
    assert!(s.pmfw.owes_answer);
}

/// A session whose boot already sent its first SMU message.
fn asked() -> Session {
    let mut s = Session::default();
    s.smu_asked = true;
    s
}

#[test]
fn a_hung_smu_gets_no_message() {
    let rig = Rig::new();
    let mut s = asked();
    let mut m = model();
    m.c90 = 0xFB;
    let (d, m) = rig.run(&mut s, Stage::Smu, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("Fehlerzustand"), "{}", d.line);
    assert!(m.messages.is_empty() && d.totals.writes == 1, "{:?}", m.writes);
    // The codes Linux sends after (OK, a failed or unknown command, a bad prerequisite, busy; smu_cmn.c:514-522
    // with the decode :249-292) do not stop it.
    for code in [0x1, 0xFF, 0xFE, 0xFD, 0xFC] {
        let mut m = model();
        m.c90 = code;
        let (d, _) = rig.run(&mut asked(), Stage::Smu, m, &mut Stick::full());
        assert!(d.pass, "{code:#x}: {}", d.line);
    }
}

/// The secure OS's version in the stick's file (its descriptor's second word).
fn old_sos(stick: &mut Stick, v: u32) {
    let b = stick.0.get_mut(&stick_path(fwlist::SOS)).unwrap();
    let at = (0..11).map(|i| 36 + i * 16).find(|at| b[*at..*at + 4] == 1u32.to_le_bytes()).unwrap();
    b[at + 4..at + 8].copy_from_slice(&v.to_le_bytes());
}

#[test]
fn below_secure_os_0x3a0e14_the_reserved_region_is_not_asked() {
    let rig = Rig::new();
    let mut stick = Stick::full();
    old_sos(&mut stick, 0x003A_0E13);
    let mut m = model();
    m.sos_version = 0x003A_0E13;
    let mut s = Session::default();
    let (_, m) = rig.cold_pass(&mut s, m, &mut stick, &ORDER);
    assert_eq!(m.commands.first(), Some(&crate::gpucore::psp::cmd::LOAD_TOC));
    assert!(!m.commands.contains(&0x50) && !m.commands.contains(&0x51) && m.commands.len() == 29 && m.next().is_none());
}

#[test]
fn without_the_secure_os_file_the_ring_stage_writes_nothing() {
    let rig = Rig::new();
    let (mut s, m) = rig.up_to(&[Stage::Sos]);
    let mut stick = Stick::full();
    stick.0.remove(&stick_path(fwlist::SOS));
    let writes = m.writes.len();
    let (d, m) = rig.run(&mut s, Stage::Ring, m, &mut stick);
    assert!(!d.pass && d.line.contains("fehlt auf dem Stick") && m.writes.len() == writes, "{}", d.line);
}

/// A cold card whose secure OS nobody loaded (C2PMSG_81 0, as the trace shows it before Linux's nine loads): the
/// look at it and the wait for its bootloader pass, every stage behind the load refuses without a write, and the
/// SMU, independent of it, still answers.
#[test]
fn without_the_load_of_the_secure_os_no_later_stage_writes() {
    let rig = Rig::new();
    let mut s = Session::default();
    let mut stick = Stick::full();
    let (done, mut m) = rig.cold_pass(&mut s, model(), &mut stick, &[Stage::Alive, Stage::Bootloader]);
    assert!(done[0].lines.iter().any(|l| l.starts_with("secure OS: not running")), "{:?}", done[0].lines);
    for st in [Stage::Ring, Stage::Toc, Stage::Tmr, Stage::Fw, Stage::Autoload, Stage::FwRest, Stage::Ta, Stage::Destroy] {
        let (d, back) = rig.run(&mut s, st, m, &mut stick);
        m = back;
        assert!(!d.pass && d.totals.writes == 0, "{st:?}: {}", d.line);
    }
    assert!(m.writes.is_empty() && m.refused.is_empty() && m.ring.is_none(), "{:?} {:?}", m.writes, m.refused);
    let (d, m) = rig.run(&mut s, Stage::Smu, m, &mut stick);
    assert!(d.pass && m.refused.is_empty(), "{:?}", d.lines);
}

/// Everything the stages put on the ring is on the ring's allow list; the command page never sees another type or
/// command.
#[test]
fn every_command_of_the_stages_is_on_the_rings_allow_list() {
    use crate::gpucore::psp::cmd;
    use rdna4_core::refuse::ring_command_refused;
    let f = files();
    let refs: Vec<&[u8]> = f.iter().map(Vec::as_slice).collect();
    for u in fwlist::list(&refs).unwrap() {
        assert_eq!(ring_command_refused(cmd::LOAD_IP_FW, &[0, 0, u.size(), u.id.fw_type()]), None, "{:?}", u.id);
    }
    assert_eq!(ring_command_refused(cmd::LOAD_IP_FW, &[0, 0, 107, crate::rdna4::stage_fw::FW_TYPE_REG_LIST]), None);
    for id in [0x50, 0x51, cmd::LOAD_TOC, cmd::AUTOLOAD_RLC, crate::rdna4::stage_fw::LOAD_ASD, cmd::LOAD_TA] {
        assert_eq!(ring_command_refused(id, &[0; 6]), None, "{id:#x}");
    }
    // The commands the model saw from the whole chain are these and no others.
    let (_, m) = Rig::new().up_to(&ORDER);
    for id in &m.commands {
        assert!(rdna4_core::refuse::RING_COMMANDS.iter().any(|(c, _)| c == id), "{id:#x}");
    }
}

/// host/remote-desk's step E06 tells the round how many register writes the look makes: the number is the
/// look's own on the fake card.
#[test]
fn the_smu_looks_writes_are_what_the_round_is_told() {
    let rig = Rig::new();
    let (d, _) = rig.run(&mut Session::default(), Stage::Smu, model(), &mut Stick::full());
    let checks = include_str!("../../../../../host/remote-desk/checks.json");
    let note = std::format!("({} register writes, each a question, no setting)", d.totals.writes);
    assert!(d.pass && checks.contains(&note), "E06's note does not say {note}");
}
