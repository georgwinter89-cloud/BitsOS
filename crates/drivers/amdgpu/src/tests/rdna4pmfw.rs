//! Wave 43: the stage `pmfw` on the model: Linux's start of the power controller equals the trace's section smu
//! with three declared rules; what the start learned stays in the session; a stop names its message and leaves
//! nothing half-opened; and every refusal of the stage, of its guard and of the model's power controller, each
//! shown by doing the thing Linux would not do.

use super::rdna4fix::*;
use super::rdna4nb::regs;
use crate::gpucore::psp::Hw;
use crate::gpucore::trail::Access;
use crate::rdna4::attach::{LinkHost, LinkNow};
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::fakepmfw::{ENABLE_READS, FEATURES};
use crate::rdna4::linuxseq::compare::{Compare, Kind};
use crate::rdna4::linuxseq::fold::Bus;
use crate::rdna4::linuxseq::{rules::Rule, section};
use crate::rdna4::stage::{Done, Session, Stage};
use crate::rdna4::chainseq::{self, Chosen};
use crate::rdna4::{Card, stage_pmfw};
use rdna4_core::card::CARD_A;
use rdna4_core::pptable14::PcieLevels;
use rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS;
use rdna4_core::scpm::{self, Scpm};
use rdna4_core::smuseq::{Stopped, Tables};
use std::string::String;
use std::vec::Vec;

/// A session and the model after Linux's chain up to `ta`: the secure processor loaded the SMU's firmware.
pub fn loaded(rig: &Rig) -> (Session, Model) {
    rig.up_to(&ORDER)
}

pub fn pmfw(rig: &Rig, s: &mut Session, m: Model) -> (Done, Model) {
    rig.run(s, Stage::Pmfw, m, &mut Stick::full())
}

pub fn said<'a>(d: &'a Done, start: &str) -> Vec<&'a str> {
    d.lines.iter().map(String::as_str).filter(|l| l.starts_with(start)).collect()
}

/// One message on the model as Linux's `smu_msg_v1_send_msg` puts it on the bus (with the pre-poll): whether the
/// model took the message write.
pub fn raw(m: &mut Model, msg: u32, arg: u32) -> bool {
    let r = m.regs.clone();
    let _ = m.reg_read(r.smu_answer);
    m.reg_write(r.smu_answer, 0) && m.reg_write(r.smu_argument, arg) && m.reg_write(r.smu_message, msg)
}

/// Linux's messages up to and with the one numbered `upto` (from 1), put on the model raw; the model must take
/// them all.
const START: [(u32, u32); 12] = [(0x03, 0), (0x02, 0), (0x0E, 0x80), (0x0F, 0x0600_0000), (0x10, 0x80), (0x11, 0x0610_0000), (0x12, 1), (0x36, 0), (0x20, 0x406), (0x20, 0x1_0406), (0x20, 0x2_0406), (0x06, 0)];

pub fn raw_start(m: &mut Model, upto: usize) {
    for (i, (msg, arg)) in START[..upto].iter().enumerate() {
        assert!(raw(m, *msg, *arg), "message {} ({msg:#x}): {:?}", i + 1, m.refused);
        // EnableAllSmuFeatures shows its answer late, as it did to Linux.
        let answer = m.regs.smu_answer;
        while m.reg_read(answer) == Some(0) {}
    }
    assert!(m.refused.is_empty());
}

pub fn last(m: &Model) -> &str {
    m.refused.last().map_or("", String::as_str)
}

#[test]
fn the_stage_equals_linuxs_section_and_only_the_declared_rules_make_it_so() {
    let rig = Rig::new();
    let (mut s, m) = loaded(&rig);
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(d.pass && d.refused.is_empty() && m.refused.is_empty() && d.trail_lost == 0, "{} {:?}", d.line, m.refused);
    assert_eq!(d.line, "Grafikkarte 1 (zeigt das Bild): Energieverwaltung starten: fertig.");
    assert_eq!(said(&d, "linuxseq"), ["linuxseq smu: equal"]);
    assert_eq!(d.lines.last().map(String::as_str), Some("linuxseq smu: equal"));
    let l = s.layout.clone().unwrap();
    let (sec, bus) = (section("smu").unwrap(), Bus::of(&regs(&rig)));
    // The tables' places come from the VRAM manager's own record, not from what the stage kept.
    let placed = crate::rdna4::pmfw::placed(&l, s.bump.as_ref().unwrap()).unwrap();
    let (tools, driver) = s.pmfw.tables.clone().unwrap();
    assert_eq!(placed, Tables { driver_mc: l.mc(driver.start), tools_mc: l.mc(tools.start) });
    assert_eq!((driver.end - driver.start, tools.end - tools.start), (0x4000, 0x19000));
    let rules = stage_pmfw::linux_rules(&l, placed);
    let mut c = Compare::new(&sec, &rules).unwrap();
    c.feed_trail(&d.trail, 0, &bus).unwrap();
    c.finish().unwrap();
    let chosen = |taken| Chosen { l: Some(&l), taken, ..Chosen::default() };
    assert_eq!(chainseq::rules("smu", l.vram_bytes, chosen(s.bump.as_ref())), Ok(rules.to_vec()));
    // Without the manager's record the section's addresses have no rule, and the comparison does not pass.
    let no_record = Err("the VRAM manager's record does not hold what the section names, so the section's addresses have no rule");
    assert_eq!(chainseq::rules("smu", l.vram_bytes, chosen(None)), no_record);
    let empty = crate::rdna4::vram::Bump::new(&l);
    assert_eq!(chainseq::rules("smu", l.vram_bytes, chosen(Some(&empty))), no_record);
    let unplaced = chainseq::verdict(&mut chainseq::Seen::new(), Stage::Pmfw, true, &d.trail, 0, &regs(&rig), rig.att.vram_bytes, chosen(None));
    assert_eq!(unplaced.as_deref(), Some("linuxseq smu: not compared: the VRAM manager's record does not hold what the section names, so the section's addresses have no rule"));
    // Without the rule for the firmware flags the read through the index pair is an access Linux's trace lacks.
    let no_flags: Vec<Rule> = rules.iter().filter(|r| !matches!(r, Rule::Untraced { .. })).cloned().collect();
    let e = Compare::new(&sec, &no_flags).unwrap().feed_trail(&d.trail, 0, &bus).unwrap_err();
    assert_eq!((e.kind, e.at), (Kind::Extra, Some((3, 893))), "{e}");
    // Without a table's rule its address is a value Linux did not write.
    for (keep, line) in [(0x10, 910), (0x0E, 924)] {
        let part: Vec<Rule> = rules.iter().filter(|r| !matches!(r, Rule::Argument(a) if a.hi != keep)).cloned().collect();
        let e = Compare::new(&sec, &part).unwrap().feed_trail(&d.trail, 0, &bus).unwrap_err();
        assert_eq!((e.kind, e.at.map(|a| a.1)), (Kind::Value, Some(line)), "{e}");
    }
    // The rule for the flags asks for the read: a stage that leaves it out differs.
    let e = Compare::new(&sec, &rules).unwrap().feed_trail(&d.trail[3..], 0, &bus).unwrap_err();
    assert!(e.kind == Kind::Missing && e.text.contains("through the index pair"), "{e}");
    // A table's address must be a page of VRAM with room for the table.
    let mut bad = d.trail.clone();
    let lo = bad.iter().position(|a| matches!(a, Access::Write { value, .. } if *value == s.pmfw.facts.table.map_or(0, |_| l.mc(s.pmfw.tables.clone().unwrap().1.start) as u32))).unwrap();
    if let Access::Write { value, .. } = &mut bad[lo] {
        *value += 4;
    }
    let e = Compare::new(&sec, &rules).unwrap().feed_trail(&bad, 0, &bus).unwrap_err();
    assert!(e.kind == Kind::Value && e.text.contains("not aligned"), "{e}");
    // And lie inside VRAM: a high half that points elsewhere is no table's address.
    let mut bad = d.trail.clone();
    let hi = bad.iter().position(|a| matches!(a, Access::Write { byte, value: 0x80, .. } if *byte == regs(&rig).smu_argument)).unwrap();
    if let Access::Write { value, .. } = &mut bad[hi] {
        *value = 0x90;
    }
    let e = Compare::new(&sec, &rules).unwrap().feed_trail(&bad, 0, &bus).unwrap_err();
    assert!(e.kind == Kind::Value && e.text.contains("is not inside"), "{e}");
    // And be the page the VRAM manager handed out for that table: the two tables' addresses changed places are
    // two well-formed addresses in free VRAM, and not what was taken.
    let los: Vec<usize> = d.trail.iter().enumerate().filter(|(_, a)| matches!(a, Access::Write { byte, value, .. } if *byte == regs(&rig).smu_argument && [placed.driver_mc as u32, placed.tools_mc as u32].contains(value))).map(|(i, _)| i).collect();
    assert_eq!(los.len(), 2, "the two low halves");
    let mut bad = d.trail.clone();
    for (i, v) in [(los[0], placed.tools_mc as u32), (los[1], placed.driver_mc as u32)] {
        if let Access::Write { value, .. } = &mut bad[i] {
            *value = v;
        }
    }
    let e = Compare::new(&sec, &rules).unwrap().feed_trail(&bad, 0, &bus).unwrap_err();
    assert!(e.kind == Kind::Value && e.at.map(|a| a.1) == Some(917) && e.text.contains("is not the one BitsOS's VRAM manager handed out for it"), "{e}");
    // Another free page than the one taken (the page behind the driver table).
    let mut bad = d.trail.clone();
    if let Access::Write { value, .. } = &mut bad[los[0]] {
        *value += 0x4000;
    }
    let e = Compare::new(&sec, &rules).unwrap().feed_trail(&bad, 0, &bus).unwrap_err();
    assert!(e.kind == Kind::Value && e.text.contains("is not the one BitsOS's VRAM manager handed out for it"), "{e}");
    // A stage that kept other places than it took is told so by its own answer's line.
    let rules_swapped = stage_pmfw::linux_rules(&l, Tables { driver_mc: placed.tools_mc, tools_mc: placed.driver_mc });
    assert!(Compare::new(&sec, &rules_swapped).unwrap().feed_trail(&d.trail, 0, &bus).is_err());
    // What went over the bus: the flags through the pair, 125 messages of three writes, four thermal registers.
    assert_eq!(d.totals.writes, 1 + 3 * 125 + 4);
    assert_eq!(m.messages.len(), 125);
    assert_eq!(m.index, SMN_MP1_FIRMWARE_FLAGS);
}

#[test]
fn the_first_message_does_not_look_at_the_answer_register_and_every_later_one_does() {
    let rig = Rig::new();
    let (mut s, m) = loaded(&rig);
    // What the reloaded PMFW's answer register holds no trace shows: the model holds 0, so a wait before the
    // first message would run out.
    assert_eq!(m.c90, 0);
    let (d, _) = pmfw(&rig, &mut s, m);
    assert!(d.pass);
    let r = regs(&rig);
    let is_wait = |a: &Access| matches!(a, Access::Wait { byte, cond: crate::gpucore::psp::Cond::Changed { from: 0 }, bound: crate::gpucore::psp::Bound::Reads(2_000_000), .. } if *byte == r.smu_answer);
    let clears: Vec<usize> = d.trail.iter().enumerate().filter(|(_, a)| matches!(a, Access::Write { byte, value: 0, .. } if *byte == r.smu_answer)).map(|(i, _)| i).collect();
    assert_eq!(clears.len(), 125);
    // The flags (index written, index read, data read), then at once the first message's three writes.
    assert_eq!(clears[0], 3);
    assert!(d.trail[..3].iter().all(|a| !is_wait(a)));
    // Every later message: Linux's wait on the answer register right before the 0 goes into it.
    assert!(clears[1..].iter().all(|i| is_wait(&d.trail[i - 1])));
    // And one after every message: 125 + 124 waits, each Linux's condition and bound.
    assert_eq!(d.trail.iter().filter(|a| is_wait(a)).count(), 249);
    assert_eq!(d.trail.iter().filter(|a| matches!(a, Access::Wait { .. })).count(), 249);
    // EnableAllSmuFeatures took as long as it took Linux: the wait went on reading.
    let longest = d.trail.iter().filter_map(|a| if let Access::Wait { reads, .. } = a { Some(*reads) } else { None }).max();
    assert_eq!(longest, Some(ENABLE_READS + 1));
    assert!(s.smu_asked, "a look after it waits for the answer register like any later message");
}

#[test]
fn what_the_start_learned_stays_in_the_session_for_the_stages_after_it() {
    let rig = Rig::new();
    let (mut s, m) = loaded(&rig);
    assert!(!m.gc_powered() && !s.pmfw.gc_powered() && !s.passed(Stage::Pmfw) && s.next() == Some(Stage::Pmfw));
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(d.pass);
    // The graphics block has power once EnableAllSmuFeatures was answered: on the card and in the session.
    assert!(m.gc_powered() && s.pmfw.gc_powered() && s.passed(Stage::Pmfw) && s.next() == Some(Stage::GcBoot));
    let f = &s.pmfw.facts;
    assert_eq!((f.fw_if, f.version, f.supported, f.dpm_running), (Some(0x33), Some(0x0068_5000), Some(FEATURES), Some(true)));
    // The clocks' levels as the card answered Linux (MHz), GFXCLK's top capped to the table's GameClockAc.
    let mhz = |c: &str| s.pmfw.facts.dpm.iter().find(|t| t.clock == c).map(|t| (t.fine_grained, t.mhz.clone()));
    assert_eq!(mhz("GFXCLK"), Some((true, std::vec![500, 2400])));
    assert_eq!(mhz("UCLK"), Some((false, std::vec![96, 456, 772, 875, 1124, 1258])));
    assert_eq!(mhz("DCEFCLK"), Some((true, std::vec![147, 1800])));
    assert_eq!(f.dpm.iter().map(|t| t.clock).collect::<Vec<_>>(), ["SOCCLK", "GFXCLK", "UCLK", "FCLK", "VCLK", "DCLK", "DCEFCLK"]);
    assert_eq!(f.pcie, ([0x0406, 0x1_0406, 0x2_0406], 3));
    assert_eq!(f.thermal.map(|t| (t.software_shutdown_temp, t.min)), Some((118, -273_150)));
    assert!(!m.pmfw.vcn_up && !m.pmfw.jpeg_up && m.pmfw.ih_allowed && m.pmfw.thermal == 4);
    assert_eq!((m.pmfw.thm_ctrl, m.pmfw.thm_ena, m.pmfw.sw_int, m.pmfw.sw_int_ctrl), (0xB8FF_0076, 0x38, 0xFE, 0));
    // The answer says it in words.
    for want in ["smu version 104.80.0", "features on after EnableAllSmuFeatures: 0x048cf19e38fffcfb", "clock GFXCLK: 2 levels [500, 2400] MHz", "thermal alert: THM_THERMAL_INT_CTRL 0x2fff0071 -> 0xb8ff0076", "the graphics block has power", "pmfw table's own header places its parts"] {
        assert!(d.lines.iter().any(|l| l.contains(want)), "{want}: {:?}", said(&d, "smu version"));
    }
    assert!(s.summary().contains("power controller started, graphics block powered, running features 0x048cf19e38fffcfb, 7 clock tables"), "{}", s.summary());
    // Once per load of the PMFW's firmware: a second start is refused before any access.
    let writes = m.writes.len();
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(!d.pass && d.trail.is_empty() && d.totals.writes == 0 && m.writes.len() == writes && d.line.contains("schon einmal gestartet"), "{}", d.line);
    // The ring's destroy leaves all of it: the PMFW runs on. A look then reads what runs, with Linux's pre-poll.
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Destroy, Stage::Smu]);
    assert!(s.pmfw.passed && s.pmfw.facts.dpm.iter().any(|t| t.clock == "UCLK") && s.pmfw.gc_powered());
    let look = s.smu.as_ref().unwrap();
    assert_eq!((look.answer(0x0D, 0), look.answer(0x0C, 0)), (Some((FEATURES >> 32) as u32), Some(FEATURES as u32)));
    // The firmware loaded again (a new ring, then fw): a new PMFW, told nothing; the start is made again, with
    // the tables at the places taken before.
    let tables = s.pmfw.tables.clone();
    let (_, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Ring, Stage::Toc, Stage::Tmr, Stage::Fw]);
    assert!(!s.pmfw.passed && !s.pmfw.began && !s.pmfw.gc_powered() && s.pmfw.facts.dpm.is_empty() && !m.gc_powered() && !s.smu_asked);
    assert_eq!(s.pmfw.tables, tables);
    let (done, m) = rig.pass(&mut s, m, &mut Stick::full(), &[Stage::Autoload, Stage::FwRest, Stage::Ta, Stage::Pmfw]);
    assert!(m.gc_powered() && s.pmfw.passed && s.pmfw.tables == tables && said(&done[3], "linuxseq") == ["linuxseq smu: equal"]);
    assert_eq!(s.bump.as_ref().unwrap().taken.len(), 4, "no table was taken a second time");
}

/// A rig whose attach read `db` as the card's runtime database.
fn rig_with_db(db: Vec<u8>) -> Rig {
    let (card, _) = Card::attach(super::rdna4::facts(&CARD_A), FakeCard { runtime_db: db, ..FakeCard::measured() });
    Rig { facts: card.facts, att: card.state.unwrap(), display: true, inherited: None }
}

#[test]
fn the_stage_refuses_before_any_access_what_it_cannot_start_as_linux_does() {
    // A card whose runtime database does not say SCPM: Linux would send messages no trace of this card holds.
    for (db, want) in [(scpm::synthetic(0).to_vec(), Scpm::Disabled), (Vec::new(), Scpm::NoDatabase { cookie: 0 })] {
        let rig = rig_with_db(db);
        assert_eq!(rig.att.scpm, want);
        let (mut s, m) = loaded(&rig);
        let writes = m.writes.len();
        let (d, m) = pmfw(&rig, &mut s, m);
        assert!(!d.pass && d.trail.is_empty() && m.writes.len() == writes && m.messages.is_empty(), "{}", d.line);
        assert!(d.line.contains("Energietabelle") && d.lines.last().unwrap().contains("SetAllowedFeaturesMaskHigh/Low"), "{:?}", d.lines);
        assert!(!s.pmfw.began && s.pmfw.tables.is_none(), "nothing was taken for a start that is not made");
        assert_eq!(said(&d, "linuxseq"), [""; 0], "a call refused before any access has no comparison");
    }
    // A host where Linux lets the PMFW switch the link's speed: another computation, not built.
    let mut rig = Rig::new();
    rig.facts.link_host = LinkHost::Switching;
    let (mut s, m) = loaded(&rig);
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(!d.pass && d.trail.is_empty() && m.messages.is_empty() && d.line.contains("nicht gebaut"), "{}", d.line);
    // A question of the look that has no answer: Linux's first message would be written over it.
    let rig = Rig::new();
    let (mut s, mut m) = loaded(&rig);
    m.smu_silent_on = Some(0x02);
    let (d, mut m) = rig.run(&mut s, Stage::Smu, m, &mut Stick::full());
    assert!(!d.pass && s.pmfw.owes_answer);
    m.smu_silent_on = None;
    let sent = m.messages.len();
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(!d.pass && d.trail.is_empty() && m.messages.len() == sent && d.line.contains("alten Frage"), "{}", d.line);
    // The second card stays untouched.
    let mut rig = Rig::new();
    rig.display = false;
    let (d, m) = pmfw(&rig, &mut Session::default(), model());
    assert!(!d.pass && d.trail.is_empty() && m.writes.is_empty() && d.line.contains("bleibt unberührt"), "{}", d.line);
    // And without `ta` the chain's order refuses it, in the one place that does for every stage.
    let rig = Rig::new();
    let (mut s, m) = rig.up_to(&ORDER[..7]);
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(!d.pass && d.trail.is_empty() && m.messages.is_empty() && d.line.contains("zuerst die Vertrauensprogramme laden"), "{}", d.line);
}

#[test]
fn the_link_limits_come_from_the_path_as_linux_walks_it() {
    // The root port at x8, or the card's switch at 16 GT/s: Linux's other arguments, and a card that expects them.
    for (path, link, args) in [
        ([(5, 16), (5, 16), (5, 16), (5, 8)], (4, 4), [0x0404, 0x1_0404, 0x2_0404]),
        ([(5, 16), (4, 16), (5, 16), (5, 16)], (3, 6), [0x0306, 0x1_0306, 0x2_0306]),
        ([(3, 4), (5, 16), (5, 16), (5, 16)], (2, 3), [0x0203, 0x1_0203, 0x2_0203]),
    ] {
        let mut rig = Rig::new();
        rig.facts.path = path.iter().map(|(speed, width)| LinkNow { speed: *speed, width: *width }).collect();
        let (mut s, mut m) = loaded(&rig);
        m.pmfw.link = link;
        let (d, m) = pmfw(&rig, &mut s, m);
        assert!(d.pass && m.refused.is_empty(), "{path:?}: {} {:?}", d.line, m.refused);
        assert_eq!(m.pmfw.pcie, args, "{path:?}");
        // That is not Linux's own start of this machine, and the stage's answer says where it differs.
        assert!(said(&d, "linuxseq")[0].starts_with("linuxseq smu: differs: smu.txt line 46 (init.txt line 952): Linux writes 0x00000406 to MP1_SMN_C2PMSG_82; the stage writes"), "{:?}", said(&d, "linuxseq"));
    }
    // A card that expects the trace's arguments refuses another path's (the model holds Linux's own computation).
    let mut rig = Rig::new();
    rig.facts.path[3] = LinkNow { speed: 5, width: 8 };
    let (mut s, m) = loaded(&rig);
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(!d.pass && last(&m).contains("OverridePcieParameters 0x404 for level 0: Linux sends 0x406"), "{:?}", m.refused);
    // A table with a lower last level than the link: the table's.
    let rig = Rig::new();
    let (mut s, mut m) = loaded(&rig);
    m.pmfw.table.pcie = PcieLevels { gen_speed: [0, 1, 3], lane_count: [4, 4, 4], lclk_freq: [1, 2, 3] };
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(d.pass && m.pmfw.pcie == [0x0304, 0x1_0304, 0x2_0304], "{} {:x?}", d.line, m.pmfw.pcie);
}

#[test]
fn the_thermal_alert_is_linuxs_change_of_what_the_card_holds() {
    let rig = Rig::new();
    let (mut s, mut m) = loaded(&rig);
    // Another card: other masks and thresholds in the register, another shutdown temperature in its table.
    (m.pmfw.thm_ctrl, m.pmfw.sw_int, m.pmfw.sw_int_ctrl) = (0x4755_3C5A, 0x0000_0103, 0x0000_0101);
    m.pmfw.table.software_shutdown_temp = 105;
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    // Five credits, the hardware's interrupt on, both thresholds unmasked, 105 above and 0 below, the trigger's
    // mask cleared; the rest as read.
    assert_eq!(m.pmfw.thm_ctrl, 0xB055_0069);
    assert_eq!((m.pmfw.thm_ena, m.pmfw.sw_int, m.pmfw.sw_int_ctrl), (0x38, 0xFE, 0x100));
    // Linux's change of another value is another write than the trace's, and the stage's answer names it.
    assert!(d.pass);
    assert_eq!(said(&d, "linuxseq"), ["linuxseq smu: differs: smu.txt line 733 (init.txt line 1871): Linux writes 0xb8ff0076 to THM_THERMAL_INT_CTRL; the stage writes 0xb0550069, and no rule declares a difference for THM_THERMAL_INT_CTRL"]);
}

/// A stop in the stage: its plain line, its last line, what the model saw after it.
pub fn stopped(setup: impl FnOnce(&mut Model)) -> (Done, Model, Session) {
    let rig = Rig::new();
    let (mut s, mut m) = loaded(&rig);
    setup(&mut m);
    let (d, m) = pmfw(&rig, &mut s, m);
    assert!(!d.pass && d.refused.is_empty(), "{}", d.line);
    (d, m, s)
}

#[test]
fn a_stop_names_its_message_and_leaves_nothing_half_opened() {
    // EnableAllSmuFeatures gets no answer within Linux's 2000000 reads: nothing more is sent.
    let (d, m, s) = stopped(|m| m.smu_silent_on = Some(0x06));
    assert!(d.line.ends_with("angehalten: Energieverwaltung antwortet nicht."), "{}", d.line);
    assert_eq!(d.lines.last().map(String::as_str), Some("stopped: EnableAllSmuFeatures (0x06, argument 0x0): no answer within Linux's 2000000 reads of the answer register (-ETIME); nothing more is sent"));
    assert_eq!(m.messages.last(), Some(&(0x06, 0)));
    assert!(d.lines.iter().any(|l| l == "after the stop: EnableAllSmuFeatures went out; the running features were not read back (the PMFW takes no message)"), "{:?}", said(&d, "after"));
    assert!(matches!(d.trail.last(), Some(Access::Wait { reads: 2_000_000, last: Some(0), .. })));
    assert!(s.pmfw.began && !s.pmfw.passed && s.pmfw.facts.enable_sent && !s.pmfw.gc_powered());
    assert_eq!(s.pmfw.stopped, Some(Stopped::Answer { msg: 0x06, arg: 0, answer: 0 }));
    // What the stage did was Linux's; the card's last answer was not the one it gave Linux, and the line says so.
    let line = "linuxseq smu: the same as Linux's as far as the stage came (Linux goes on at init.txt line 977); 1 answers of the card are not the trace's, the first: smu.txt line 63 (init.txt line 975): Linux waits on MP1_SMN_C2PMSG_90";
    assert!(said(&d, "linuxseq").len() == 1 && said(&d, "linuxseq")[0].starts_with(line), "{:?}", said(&d, "linuxseq"));
    // It answers an error: the running features are read back and said; that read-back is outside the comparison.
    let (d, m, s) = stopped(|m| m.pmfw.answer_with = Some((0x06, 0xFD)));
    assert!(d.line.ends_with("angehalten: Energieverwaltung lehnt einen Befehl ab."), "{}", d.line);
    assert_eq!(d.lines.last().map(String::as_str), Some("stopped: EnableAllSmuFeatures (0x06, argument 0x0) answered 0xfd: Linux's -EINVAL (smu_cmn.c:249-292)"));
    assert_eq!(m.messages[m.messages.len() - 3..], [(0x06, 0), (0x0D, 0), (0x0C, 0)]);
    // (The model's PMFW did not switch its features on for a message it answers with an error.)
    assert!(d.lines.iter().any(|l| l == "after the stop: EnableAllSmuFeatures went out; running features read back 0x000000013fffffff (graphics block not known to be powered: the message was not answered OK)"), "{:?}", said(&d, "after"));
    assert!(said(&d, "linuxseq").len() == 2 && said(&d, "linuxseq")[0].starts_with(line), "{:?}", said(&d, "linuxseq"));
    assert_eq!(said(&d, "linuxseq")[1], "linuxseq: 12 accesses after the stop (what Linux's err_out powers down again, the read-back of the running features) are not compared");
    assert_eq!(s.pmfw.facts.read_back, Some(0x1_3FFF_FFFF));
    // A code Linux takes for a hang: nothing more.
    let (d, m, _) = stopped(|m| m.pmfw.answer_with = Some((0x06, 0xFB)));
    assert!(d.lines.last().unwrap().contains("answered 0xfb: Linux's -EREMOTEIO") && m.messages.last() == Some(&(0x06, 0)), "{:?}", d.lines.last());
    // A clock question fails while the video block is up: JPEG and VCN go down again, then the read-back.
    let (d, m, s) = stopped(|m| m.pmfw.answer_with = Some((0x1F, 0xFF)));
    assert!(d.lines.last().unwrap().starts_with("stopped: GetDpmFreqByIndex (0x1f, argument 0x100ff) answered 0xff: Linux's -EIO"), "{:?}", d.lines.last());
    assert_eq!(m.messages[m.messages.len() - 5..], [(0x1F, 0x1_00FF), (0x2D, 0), (0x2B, 0), (0x0D, 0), (0x0C, 0)]);
    assert!(!m.pmfw.vcn_up && !m.pmfw.jpeg_up && m.refused.is_empty() && !s.pmfw.facts.vcn_up);
    assert!(d.lines.iter().any(|l| l == "after the stop: PowerDownJpeg, PowerDownVcn answered OK (Linux's err_out, amdgpu_smu.c:915-921)"), "{:?}", said(&d, "after"));
    assert!(s.pmfw.gc_powered(), "the features are on: the session says what the card was left with");
    // PowerUpJpeg fails: VCN goes down again.
    let (_, m, _) = stopped(|m| m.pmfw.answer_with = Some((0x2C, 0xFF)));
    assert_eq!(m.messages[m.messages.len() - 4..], [(0x2C, 0), (0x2B, 0), (0x0D, 0), (0x0C, 0)]);
    assert!(!m.pmfw.vcn_up);
    // A message before EnableAllSmuFeatures fails: nothing follows, and the answer says the features were not touched.
    let (d, m, s) = stopped(|m| m.pmfw.answer_with = Some((0x36, 0xFF)));
    assert_eq!(m.messages.last(), Some(&(0x36, 0)));
    assert!(d.lines.iter().any(|l| l.starts_with("after the stop: EnableAllSmuFeatures did not go out")) && !m.gc_powered() && !s.pmfw.gc_powered());
    // A stop is final for this PMFW: the next call is refused before any access.
    let rig = Rig::new();
    let (mut s2, m2) = (s, m);
    let sent = m2.messages.len();
    let (d, m2) = pmfw(&rig, &mut s2, m2);
    assert!(!d.pass && d.trail.is_empty() && m2.messages.len() == sent);
    assert!(s2.summary().contains("power controller stopped half-way (Answer { msg: 54, arg: 0, answer: 255 })"), "{}", s2.summary());
}
