//! Wave 43: the stops of the stage `pmfw` that leave something on the card, and what the answer says of it: a
//! power-down of the video block that fails (the other one goes out all the same, as Linux's does), a stop after
//! the thermal alert's registers were written (they stay, and the answer says so), a driver table that does not
//! read out of VRAM; a firmware load that fails, which must not make the driver take the power controller
//! for a new one; and the answers Linux could not go on with, each of which stops the stage.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use super::rdna4pmfw::{loaded, pmfw, said, stopped};
use crate::gpucore::psp::{self, Bound, Cond, Hw};
use crate::gpucore::trail::Access;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakepmfw::FEATURES;
use crate::rdna4::host::Host;
use crate::rdna4::stage::{Session, Stage};
use rdna4_core::smuseq::Stopped;
use std::string::String;

fn has(d: &crate::rdna4::stage::Done, line: &str) -> bool {
    d.lines.iter().any(|l| l == line)
}

const ALERT: &str = "of the thermal alert's four registers were written (THM_THERMAL_INT_CTRL, THM_THERMAL_INT_ENA, MP1_SMN_IH_SW_INT, MP1_SMN_IH_SW_INT_CTRL, in that order) and stay as written: Linux's smu_smc_hw_setup returns without taking them back too (amdgpu_smu.c:1885-1889); with all four the alert is armed, and the thermal taker at the IH reader handles what it raises";

#[test]
fn a_power_down_that_fails_does_not_keep_the_other_one_from_the_card() {
    // PowerDownJpeg answered with an error: Linux drops that result and powers VCN down (amdgpu_smu.c:912-921).
    // The stage stops (stricter than Linux), VCN goes down all the same, then the read-back.
    let (d, m, s) = stopped(|m| m.pmfw.answer_with = Some((0x2D, 0xFF)));
    assert!(d.line.ends_with("angehalten: Energieverwaltung lehnt einen Befehl ab."), "{}", d.line);
    assert_eq!(d.lines.last().map(String::as_str), Some("stopped: PowerDownJpeg (0x2d, argument 0x0) answered 0xff: Linux's -EIO (smu_cmn.c:249-292)"));
    assert_eq!(m.messages[m.messages.len() - 4..], [(0x2D, 0), (0x2B, 0), (0x0D, 0), (0x0C, 0)]);
    // (The model's PMFW did nothing for the message it answered with an error: its JPEG stays up.)
    assert!(m.refused.is_empty() && !m.pmfw.vcn_up && m.pmfw.jpeg_up, "{:?}", m.refused);
    assert!(has(&d, "after the stop: PowerDownVcn answered OK (Linux's err_out, amdgpu_smu.c:915-921)"), "{:?}", said(&d, "after"));
    assert!(has(&d, "after the stop: the video block stays powered up (VCN false, JPEG true): a power-down that was not answered OK is not sent a second time, and a PMFW that takes no message gets none"), "{:?}", said(&d, "after"));
    assert_eq!(s.pmfw.stopped, Some(Stopped::Answer { msg: 0x2D, arg: 0, answer: 0xFF }));
    assert!(s.pmfw.facts.jpeg_up && !s.pmfw.facts.vcn_up && s.pmfw.facts.read_back == Some(FEATURES));
    // Those accesses are the stop's own, outside the comparison.
    assert_eq!(said(&d, "linuxseq").len(), 2, "{:?}", said(&d, "linuxseq"));
    assert!(said(&d, "linuxseq")[1].starts_with("linuxseq: ") && said(&d, "linuxseq")[1].contains("accesses after the stop"));
    // PowerDownVcn answered with an error: nothing is left to power down, and the answer says what stays up.
    let (d, m, s) = stopped(|m| m.pmfw.answer_with = Some((0x2B, 0xFE)));
    assert!(d.lines.last().unwrap().starts_with("stopped: PowerDownVcn (0x2b, argument 0x0) answered 0xfe: Linux's -EOPNOTSUPP"), "{:?}", d.lines.last());
    assert_eq!(m.messages[m.messages.len() - 4..], [(0x2D, 0), (0x2B, 0), (0x0D, 0), (0x0C, 0)]);
    assert!(m.refused.is_empty() && m.pmfw.vcn_up && !m.pmfw.jpeg_up);
    assert!(d.lines.iter().any(|l| l.starts_with("after the stop: the video block stays powered up (VCN true, JPEG false)")), "{:?}", said(&d, "after"));
    assert!(!d.lines.iter().any(|l| l.contains("answered OK (Linux's err_out")), "nothing was powered down after the stop");
    assert!(s.pmfw.facts.vcn_up && !s.pmfw.facts.jpeg_up);
    // No answer to PowerDownJpeg: Linux's next message would not go out either (smu_cmn.c:514-522).
    let (d, m, _) = stopped(|m| m.smu_silent_on = Some(0x2D));
    assert_eq!(m.messages.last(), Some(&(0x2D, 0)));
    assert!(d.lines.iter().any(|l| l.starts_with("after the stop: the video block stays powered up (VCN true, JPEG true)")), "{:?}", said(&d, "after"));
}

/// The model with one register whose writes do not arrive.
struct NoWrite {
    m: Model,
    byte: u32,
}

impl Hw for NoWrite {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.m.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        byte != self.byte && self.m.reg_write(byte, value)
    }
    fn mem_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.m.mem_read(at, buf)
    }
    fn mem_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.m.mem_write(at, data)
    }
    fn now_us(&mut self) -> u64 {
        self.m.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.m.delay_us(us)
    }
    fn wait_begin(&mut self, byte: u32, cond: Cond, bound: Bound) {
        self.m.wait_begin(byte, cond, bound)
    }
    fn wait_end(&mut self) {
        self.m.wait_end()
    }
}

impl Host for NoWrite {}

#[test]
fn a_stop_after_the_thermal_alerts_registers_says_that_they_stay_written() {
    // AllowIHHostInterrupt is not answered OK: the four registers were written and nothing takes them back.
    let (d, m, s) = stopped(|m| m.pmfw.answer_with = Some((0x4D, 0xFF)));
    assert!(d.lines.last().unwrap().starts_with("stopped: AllowIHHostInterrupt (0x4d, argument 0x0) answered 0xff: Linux's -EIO"), "{:?}", d.lines.last());
    assert!(has(&d, &std::format!("after the stop: 4 {ALERT}")), "{:?}", said(&d, "after"));
    assert_eq!((m.pmfw.thermal, m.pmfw.thm_ctrl, m.pmfw.thm_ena, m.pmfw.sw_int, m.pmfw.sw_int_ctrl, m.pmfw.ih_allowed), (4, 0xB8FF_0076, 0x38, 0xFE, 0, false));
    assert_eq!(m.messages[m.messages.len() - 3..], [(0x4D, 0), (0x0D, 0), (0x0C, 0)]);
    assert_eq!((s.pmfw.facts.alert_written, s.pmfw.facts.read_back), (4, Some(FEATURES)));
    // No answer to it: the same is said, and nothing more is sent.
    let (d, m, _) = stopped(|m| m.smu_silent_on = Some(0x4D));
    assert!(has(&d, &std::format!("after the stop: 4 {ALERT}")) && m.messages.last() == Some(&(0x4D, 0)), "{:?}", said(&d, "after"));
    // A write in the middle of the four that does not arrive: the count says how far it came.
    let rig = Rig::new();
    let (mut s, m) = loaded(&rig);
    let ena = regs(&rig).thm_int_ena;
    let (d, card) = rig.run_on(&mut s, Stage::Pmfw, NoWrite { m, byte: ena }, &mut Stick::full());
    assert!(!d.pass && d.line.ends_with("angehalten: ein Schreibzugriff war nicht erlaubt."), "{}", d.line);
    assert!(has(&d, &std::format!("after the stop: 1 {ALERT}")), "{:?}", said(&d, "after"));
    assert_eq!((card.m.pmfw.thermal, card.m.pmfw.thm_ctrl, s.pmfw.facts.alert_written), (1, 0xB8FF_0076, 1));
    // The bus failed the stage: nothing more is sent, the read-back neither (the last messages are Linux's own
    // power-down of the video block), and the stage tried nothing its guard had to refuse.
    assert_eq!(card.m.messages[card.m.messages.len() - 2..], [(0x2D, 0), (0x2B, 0)]);
    assert!(!card.m.messages.contains(&(0x4D, 0)) && card.m.refused.is_empty() && d.refused.is_empty(), "{:?} {:?}", card.m.refused, d.refused);
    assert!(has(&d, "after the stop: EnableAllSmuFeatures went out; the running features were not read back (nothing is sent after an answer register that did not read as answered, or after the bus failed the stage)"), "{:?}", said(&d, "after"));
    assert_eq!((s.pmfw.stopped, s.pmfw.facts.read_back), (Some(Stopped::Stage), None));
    assert_eq!(said(&d, "linuxseq").len(), 1, "the stop made no access of its own: {:?}", said(&d, "linuxseq"));
    // A stop before the alert's step says nothing of it, and neither does a start that came to its end.
    let (d, _, s) = stopped(|m| m.pmfw.answer_with = Some((0x1F, 0xFF)));
    assert!(!d.lines.iter().any(|l| l.contains("thermal alert's four registers")) && s.pmfw.facts.alert_written == 0);
    let rig = Rig::new();
    let (mut s, m) = loaded(&rig);
    let (d, _) = pmfw(&rig, &mut s, m);
    assert!(d.pass && s.pmfw.facts.alert_written == 4 && !d.lines.iter().any(|l| l.starts_with("after the stop")));
}

#[test]
fn a_driver_table_that_does_not_read_stops_the_stage_right_after_the_transfer() {
    let (d, m, s) = stopped(|m| m.pmfw.table_unreadable = true);
    assert!(d.line.ends_with("angehalten: Speicher der Karte lässt sich nicht lesen."), "{}", d.line);
    let l = s.layout.clone().unwrap();
    let driver = l.mc(s.pmfw.tables.clone().unwrap().1.start);
    assert_eq!(d.lines.last().map(String::as_str), Some(std::format!("stopped: the driver table at mc {driver:#x} does not read").as_str()));
    // Nothing after TransferTableSmu2Dram: no RunDcBtc, no link level, no feature.
    assert_eq!(m.messages.last(), Some(&(0x12, 1)));
    assert_eq!(m.messages.len(), 7);
    assert!(m.pmfw.transferred && !m.pmfw.btc && m.pmfw.pcie.is_empty() && !m.gc_powered() && m.refused.is_empty());
    assert_eq!((s.pmfw.stopped, s.pmfw.facts.table, s.pmfw.facts.enable_sent), (Some(Stopped::Stage), None, false));
    assert!(s.pmfw.began && !s.pmfw.passed);
    assert!(has(&d, "after the stop: EnableAllSmuFeatures did not go out; the PMFW's features are as its own start left them"), "{:?}", said(&d, "after"));
    assert!(!d.lines.iter().any(|l| l.starts_with("pmfw table:")), "no table was parsed");
    // What the stage did up to there was Linux's, and the stop sent nothing of its own.
    assert_eq!(said(&d, "linuxseq").len(), 1, "{:?}", said(&d, "linuxseq"));
    assert!(said(&d, "linuxseq")[0].starts_with("linuxseq smu: the same as Linux's as far as the stage came"), "{:?}", said(&d, "linuxseq"));
}

/// The first access of a look `smu` on the answer register: Linux's wait before a message (the pre-poll), or
/// the 0 written without looking.
fn look_begins_with_the_wait(d: &crate::rdna4::stage::Done, answer: u32) -> bool {
    let first = d.trail.iter().find(|a| matches!(a, Access::Wait { byte, .. } | Access::Write { byte, .. } if *byte == answer));
    matches!(first, Some(Access::Wait { cond: Cond::Changed { from: 0 }, .. }))
}

#[test]
fn a_firmware_load_that_fails_leaves_the_power_controller_the_one_it_was() {
    let answer = regs(&Rig::new()).smu_answer;
    // A look's question that the power controller did not answer, then a new ring and `fw`, whose first image is
    // the SMU's firmware: `kind` makes that load fail, by the secure processor's status or by no answer at all.
    for kind in ["status", "hang"] {
        let rig = Rig::new();
        let mut stick = Stick::full();
        let (mut s, mut m) = rig.up_to(&[Stage::Sos, Stage::Ring, Stage::Toc, Stage::Tmr]);
        m.smu_silent_on = Some(0x02);
        let (d, mut m) = rig.run(&mut s, Stage::Smu, m, &mut stick);
        assert!(!d.pass && s.pmfw.owes_answer && s.smu_asked, "{kind}: {}", d.line);
        m.smu_silent_on = None;
        match kind {
            // Another image than the card expects: the model's secure processor answers a status.
            "status" => m.expect.images[0].1[0] ^= 0xFF,
            _ => m.hang_on = Some(psp::cmd::LOAD_IP_FW),
        }
        let (d, mut m) = rig.run(&mut s, Stage::Fw, m, &mut stick);
        assert!(!d.pass && d.totals.writes > 0 && s.loaded.is_empty(), "{kind}: {} {:?}", d.line, s.loaded);
        // It is not known that a new PMFW runs: the old question is still owed, and the next message waits.
        assert!(s.pmfw.owes_answer && s.smu_asked, "{kind}: owes {} asked {}", s.pmfw.owes_answer, s.smu_asked);
        m.refused.clear();
        let sent = m.messages.len();
        let (d, m) = rig.run(&mut s, Stage::Smu, m, &mut stick);
        assert!(look_begins_with_the_wait(&d, answer), "{kind}: the look wrote without Linux's wait: {:?}", &d.trail[..d.trail.len().min(6)]);
        // The answer register still shows no answer: nothing is written over the message that may still run.
        assert!(!d.pass && m.messages.len() == sent && m.refused.is_empty() && d.totals.writes <= 1, "{kind}: {} {:?} {:?}", d.line, m.refused, &m.messages[sent..]);
        assert!(s.pmfw.owes_answer);
    }
    // The load answered: a new PMFW, which owes nothing; the next message is its first and waits for nothing.
    let rig = Rig::new();
    let mut stick = Stick::full();
    let (mut s, mut m) = rig.up_to(&[Stage::Sos, Stage::Ring, Stage::Toc, Stage::Tmr]);
    m.smu_silent_on = Some(0x02);
    let (_, mut m) = rig.run(&mut s, Stage::Smu, m, &mut stick);
    assert!(s.pmfw.owes_answer && s.smu_asked);
    m.smu_silent_on = None;
    let (_, m) = rig.pass(&mut s, m, &mut stick, &[Stage::Fw]);
    assert!(!s.pmfw.owes_answer && !s.smu_asked);
    let (d, m) = rig.run(&mut s, Stage::Smu, m, &mut stick);
    assert!(d.pass && !look_begins_with_the_wait(&d, answer) && m.refused.is_empty(), "{} {:?}", d.line, m.refused);
    // A `fw` that is refused before any access (it ran for this ring) resets nothing.
    let mut s2 = Session::default();
    let (_, m2) = rig.cold_pass(&mut s2, model(), &mut stick, &[Stage::Sos, Stage::Ring, Stage::Toc, Stage::Tmr, Stage::Fw, Stage::Autoload, Stage::FwRest, Stage::Ta, Stage::Pmfw]);
    let (d, _) = rig.run(&mut s2, Stage::Fw, m2, &mut stick);
    assert!(!d.pass && d.totals.writes == 0 && s2.pmfw.passed && s2.pmfw.began && s2.smu_asked, "{}", d.line);
}

#[test]
fn answers_linux_could_not_go_on_with_stop_the_stage() {
    // A table that is none: nothing after the transfer is sent.
    let (d, m, _) = stopped(|m| m.pmfw.table.structuresize = 0);
    assert!(d.line.contains("nicht die, die Linux' Vorlage beschreibt") && d.lines.last().unwrap().contains("Table(Empty); nothing after TransferTableSmu2Dram was sent"), "{:?}", d.lines.last());
    assert_eq!(m.messages.last(), Some(&(0x12, 1)));
    // A link level outside what the header documents.
    let (d, m, _) = stopped(|m| m.pmfw.table.pcie.gen_speed[2] = 7);
    assert!(d.lines.last().unwrap().contains("PcieGen { level: 2, value: 7 }") && m.messages.last() == Some(&(0x12, 1)), "{:?}", d.lines.last());
    // A clock's feature that runs at one question and not at the next (the 12th half asked is SOCCLK's count's).
    let (d, m, _) = stopped(|m| m.pmfw.features_from = Some((11, FEATURES & !(1 << 5))));
    assert!(d.lines.last().unwrap().contains("FeatureWentOff { clock: \"SOCCLK\" }"), "{:?}", d.lines.last());
    assert!(!m.pmfw.vcn_up && !m.pmfw.jpeg_up && m.refused.is_empty(), "the video block is powered down again: {:?}", m.refused);
    // More levels than Linux's table holds.
    let (d, m, _) = stopped(|m| m.pmfw.levels_of = Some((2, 17)));
    assert!(d.lines.last().unwrap().contains("Levels { clock: \"UCLK\", count: 17 }") && !m.pmfw.vcn_up, "{:?}", d.lines.last());
    // A firmware that does not run gets no message.
    let (d, m, s) = stopped(|m| m.smu_flags = 0);
    assert!(d.line.contains("läuft nicht") && m.messages.is_empty() && !s.pmfw.began, "{}", d.line);
}
