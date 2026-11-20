//! Wave 44, item ME: what MES's stages do when MES does not answer as Linux's start expects. A command whose
//! failure ends Linux's start (`goto failure`) stops the stage with a line that names the command and the pipe;
//! the two SET_HW_RESOURCES_1, whose answers Linux does not check, are said and passed over; a stage that stopped
//! after it wrote a register does not run again in this boot.

use super::rdna4fix::*;
use super::rdna4mes::before_mes;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::stage::{Done, Session, Stage};
use rdna4_core::mes12::{self, Pipe};
use std::vec::Vec;

/// MES's start on a model the test prepared.
fn mes_on(rig: &Rig, prep: impl FnOnce(&mut Model)) -> (Session, Model, Done) {
    let (mut s, mut m) = before_mes(rig);
    prep(&mut m);
    let (d, m) = rig.run(&mut s, Stage::Mes, m, &mut Stick::full());
    (s, m, d)
}

fn line<'a>(d: &'a Done, pat: &str) -> &'a str {
    d.lines.iter().find(|l| l.contains(pat)).map(|l| l.as_str()).unwrap_or_else(|| panic!("no line with {pat}: {:?}", d.lines))
}

#[test]
fn a_pipe_that_never_answers_stops_the_stage_at_its_first_command_after_linuxs_bound() {
    let rig = Rig::new();
    let (mut s, m, d) = mes_on(&rig, |m| m.mes.dead[Pipe::Kiq.index()] = true);
    assert!(!d.pass && d.line.ends_with("angehalten: der Planer der Karte antwortet nicht."), "{}", d.line);
    let said = line(&d, "MES(1) failed to respond to msg=SET_HW_RSRC");
    assert!(said.contains("no fence after 1050000 polls 2 us apart (Linux's 2.1 s)"), "{said}");
    assert!(d.lines.last().unwrap().starts_with("stopped: MES(1) failed to respond to msg=SET_HW_RSRC"), "{:?}", d.lines.last());
    // The registers before it are Linux's: the comparison holds as far as the stage came.
    assert!(d.lines.iter().any(|l| l.starts_with("linuxseq mes: the same as Linux's as far as the stage came")), "{:?}", d.lines);
    // Linux's bound passed on the model's clock: 2.1 s of 2 us polls.
    assert!(m.card.now_us >= mes12::TIMEOUT_US, "{}", m.card.now_us);
    assert!(s.mes.tried && !s.mes.ready());
    // Not again in this boot: the pipes are in no state of Linux's cold start.
    let (d, _) = rig.run(&mut s, Stage::Mes, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.line.contains("blieb stehen"), "{}", d.line);
}

#[test]
fn a_failed_answer_stops_where_linux_stops_and_only_there() {
    let rig = Rig::new();
    // The frames of MES's start: 0 KIQ SET_HW_RSRC, 2 KIQ _1, 4 ADD_QUEUE of the scheduler's queue, 6 the
    // scheduler's SET_HW_RSRC, 8 its _1, 10 QUERY_SCHEDULER_STATUS, 12 CHANGE_CONFIG (each with its QUERY behind it).
    for (frame, stops, said) in [
        (0, Some("der Planer der Karte antwortet nicht"), "MES(1) failed to respond to msg=SET_HW_RSRC: the fence came after 0 polls, the status is 0x0"),
        (2, None, "MES(1) failed to respond to msg=SET_HW_RSRC_1"),
        (4, Some("der Planer der Karte antwortet nicht"), "MES(1) failed to respond to msg=ADD_QUEUE"),
        (6, Some("der Planer der Karte antwortet nicht"), "MES(0) failed to respond to msg=SET_HW_RSRC"),
        (8, None, "MES(0) failed to respond to msg=SET_HW_RSRC_1"),
        (10, Some("der Planer der Karte ist beschäftigt"), "MES(0) failed to respond to msg=QUERY_SCHEDULER_STATUS"),
        (12, Some("der Planer der Karte nimmt die Einstellung nicht an"), "MES(1) failed to respond to msg=MISC (CHANGE_CONFIG)"),
    ] {
        let (s, m, d) = mes_on(&rig, |m| m.mes.status_zero = Some(frame));
        let l = line(&d, said);
        match stops {
            Some(plain) => {
                assert!(!d.pass && d.line.ends_with(&std::format!("angehalten: {plain}.")) && !s.mes.ready() && s.mes.tried, "frame {frame}: {}", d.line);
                if frame == 10 {
                    assert!(d.lines.last().unwrap().starts_with("stopped: MES is busy: MES(0) failed to respond"), "{:?}", d.lines.last());
                }
                // Nothing after the failed command went out.
                assert_eq!(m.mes.frames.len(), frame + 2, "frame {frame}");
            }
            None => {
                assert!(d.pass && s.mes.ready() && l.ends_with("Linux does not check this answer (mes_v12_0.c:2008-2011, :2076-2077) and goes on, so does this"), "frame {frame}: {} {l}", d.line);
                assert_eq!(m.mes.frames.len(), 14, "frame {frame}");
            }
        }
        assert!(m.refused.is_empty(), "{:?}", m.refused);
    }
}

#[test]
fn a_fence_that_never_reaches_the_sequence_number_is_no_answer() {
    let rig = Rig::new();
    let (s, _, d) = mes_on(&rig, |m| m.mes.wrong_fence = true);
    assert!(!d.pass && !s.mes.ready() && line(&d, "MES(1) failed to respond to msg=SET_HW_RSRC").contains("no fence after 1050000 polls"), "{}", d.line);
}

#[test]
fn a_kernel_queue_that_mes_does_not_take_stops_mesq_and_it_does_not_run_again() {
    let rig = Rig::new();
    let (mut s, mut m) = before_mes(&rig);
    // The third compute queue's ADD_QUEUE is frame 18 (14 of MES's start, then two per queue).
    m.mes.status_zero = Some(18);
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Mes]);
    let (d, m) = rig.run(&mut s, Stage::MesQ, m, &mut Stick::full());
    assert!(!d.pass && d.line.ends_with("angehalten: der Planer nimmt eine Rechen-Warteschlange nicht an.") && d.lines.last().unwrap().starts_with("stopped: failed to map compute queue: MES(0) failed to respond to msg=ADD_QUEUE"), "{}\n{:?}", d.line, d.lines);
    assert_eq!((m.cp.queues.len(), s.mes.kq, s.mes.kq_tried), (3, false, true), "the third was handed on before its answer went wrong");
    let (d, _) = rig.run(&mut s, Stage::MesQ, m, &mut Stick::full());
    assert!(!d.pass && d.trail.is_empty() && d.line.contains("schon angefasst"), "{}", d.line);
}

#[test]
fn a_pipe_without_its_firmware_is_refused_by_the_card_at_its_start() {
    let rig = Rig::new();
    let (s, m, d) = mes_on(&rig, |m| m.mes.fw = false);
    assert!(!d.pass && s.mes.tried && !s.mes.ready(), "{}", d.line);
    assert!(m.refused.len() == 1 && m.refused[0].contains("started without MES's four images loaded by the secure processor before the autoload"), "{:?}", m.refused);
    // The write that did not arrive is the scheduler pipe's start, the stage's ninth access after its bind.
    let refused: Vec<_> = d.trail.iter().filter(|a| matches!(a, crate::gpucore::trail::Access::Write { reached: false, .. })).collect();
    assert_eq!(refused.len(), 1);
    // The stop came under the scheduler pipe's selection: it is put back to 0 after the stop, outside the
    // comparison, whose line names the refused write as the first difference.
    assert_eq!(m.gc.grbm, 0);
    assert!(d.lines.iter().any(|l| l == "mes: GRBM_GFX_CNTL put back to 0 after the stop"), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l == "linuxseq: 1 accesses after the stop (the selection put back after MES's stop) are not compared"), "{:?}", d.lines);
    assert!(d.lines.iter().any(|l| l.starts_with("linuxseq mes: differs: mes.txt line 7 (init.txt line 9507): Linux writes 0x04000000 to CP_MES_CNTL; the stage tries to write 0x04000000 to CP_MES_CNTL, which is refused")), "{:?}", d.lines);
}
