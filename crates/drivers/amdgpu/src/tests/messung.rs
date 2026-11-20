//! The measurement run (`rdna4::messung`) on the fake card: from a cold card to the end, every stage equal and a
//! complete protocol; a difference injected in the middle stops the run there and no later stage touches the
//! card; an answer of the card that differs goes on; a missing firmware file and a card that is not cold stop in
//! the preflight with nothing written; the protocol after a hang holds every line up to the hung stage; the
//! power-off comes at the end in every case that does not hang. Each refusal of the stop rule has its own test.

use super::rdna4::facts;
use crate::rdna4::chainseq::{Said, Verdict};
use crate::rdna4::fakefiles::{model, warm};
use crate::rdna4::fakemodel::{Model, STAGE_STACK};
use crate::rdna4::fwlist;
use crate::rdna4::livestage::Claim;
use crate::rdna4::messfake::{Fake, Spoil};
use crate::rdna4::messung::{self, Disk, Env, Next, Outcome, RUN, Took};
use crate::rdna4::stage::{Done, Stage};
use crate::gpucore::fwload::stick_path;
use rdna4_core::card::{CARD_A, CARD_B};
use std::format;
use std::string::{String, ToString};
use std::sync::{Arc, Mutex};
use std::vec::Vec;

/// The protocol as the disk got it, shared so a test still reads it after a panic.
#[derive(Clone, Default)]
pub struct Paper {
    pub text: Arc<Mutex<String>>,
    pub refuse_open: bool,
    pub puts: Arc<Mutex<usize>>,
    /// Every `put` from this one on (from 1) fails: the disk went away in the middle of the run.
    pub fail_from: Option<usize>,
}

impl Disk for Paper {
    fn open(&mut self, head: &str) -> Result<String, String> {
        if self.refuse_open {
            return Err(String::from("the boot partition is mounted read only"));
        }
        *self.text.lock().unwrap() = String::from(head);
        Ok(String::from("GPUM0001.TXT"))
    }
    fn put(&mut self, text: &str) -> Result<(), String> {
        let mut n = self.puts.lock().unwrap();
        *n += 1;
        if self.fail_from.is_some_and(|f| *n >= f) {
            return Err(String::from("an I/O error of the stick"));
        }
        self.text.lock().unwrap().push_str(text);
        Ok(())
    }
}

/// The machine around the run: a clock that moves 1 ms a question, the screen, the power-off recorded.
#[derive(Default)]
pub struct Room {
    pub now: u64,
    /// How far the clock moves at each question (1 ms when 0).
    pub step: u64,
    pub screen: Vec<String>,
    pub offs: usize,
    pub waited: u64,
}

impl Env for Room {
    fn now_us(&mut self) -> u64 {
        self.now += if self.step == 0 { 1000 } else { self.step };
        self.now
    }
    fn log(&mut self) -> Vec<String> {
        Vec::new()
    }
    fn levels(&mut self) -> String {
        String::from("info")
    }
    fn build(&mut self) -> String {
        String::from("test")
    }
    fn say(&mut self, line: &str) {
        self.screen.push(line.to_string());
    }
    fn wait_s(&mut self, s: u64) {
        self.waited += s;
    }
    fn poweroff(&mut self) -> String {
        self.offs += 1;
        String::from("a test: the machine stays on")
    }
}

pub fn fake(m: Model) -> Fake {
    Fake::new(facts(&CARD_A), m).expect("the measured card attaches")
}

pub fn run(card: &mut Fake) -> (Outcome, String, Room) {
    let mut disk = Paper::default();
    let mut room = Room::default();
    let out = messung::go(card, &mut disk, &mut room);
    let text = disk.text.lock().unwrap().clone();
    (out, text, room)
}

fn with<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
    text.lines().filter(|l| l.split(' ').next() == Some(key)).collect()
}

/// The protocol's keys: every line starts with one (or is a heading).
pub const KEYS: [&str; 21] = ["##", "KOPF", "REGEL", "PRUEF", "FIRMWARE", "GPU", "START", "ANTWORT", "ZEIT", "STAPEL", "VERGLEICH", "REGELN", "SPURINFO", "SPUR", "IH", "THERMO", "BLICK", "LOG", "WEITER", "STOPP", "ENDE"];

#[test]
fn the_whole_run_from_a_cold_card_writes_a_complete_protocol_and_every_stage_is_linuxs() {
    let mut card = fake(model());
    let (out, text, room) = run(&mut card);
    assert_eq!(out.stopped, None, "{text}");
    assert_eq!(out.ran, RUN);
    assert_eq!(room.offs, 1, "the power-off comes at the end");
    assert_eq!(room.waited, messung::OFF_IN_S);
    for l in text.lines() {
        assert!(KEYS.contains(&l.split(' ').next().unwrap_or("")), "a line with no key: {l}");
    }
    let starts: Vec<&str> = with(&text, "START").iter().map(|l| l.split(' ').nth(1).unwrap()).collect();
    assert_eq!(starts, RUN.map(Stage::name));
    // Every stage's comparison: equal, or equal so far inside psp-ring; no answer of the card differs.
    for l in with(&text, "VERGLEICH") {
        assert!((l.contains("said=equal") || l.contains("said=so-far")) && l.ends_with("answers=0"), "{l}");
    }
    assert_eq!(with(&text, "WEITER").len(), RUN.len());
    // Wave 44: the thermal taker's counts after every stage, one read after pmfw and one at the end, all cool.
    let thermo = with(&text, "THERMO");
    assert_eq!(thermo.iter().filter(|l| l.contains(" alarme=+0:+0:+0 ")).count(), RUN.len(), "{thermo:?}");
    for when in ["pmfw", "ende"] {
        assert!(thermo.iter().any(|l| l.starts_with(&format!("THERMO {when} temperatures (one metrics transfer")) && l.contains("hotspot 38 C")), "{when}: {thermo:?}");
        assert!(thermo.iter().any(|l| l.starts_with(&format!("THERMO {when} urteil ok "))), "{when}: {thermo:?}");
    }
    assert!(with(&text, "STOPP").is_empty());
    // The preflight: every check passed, the twelve files, the cold card.
    assert!(with(&text, "PRUEF").iter().all(|l| l.split(' ').nth(2) == Some("ok")), "{text}");
    assert_eq!(with(&text, "FIRMWARE").len(), 12);
    assert!(text.contains("PRUEF kalt ok C2PMSG_81 0x00000000"));
    // Each stage's trail is there and nothing was lost; every section's rules once.
    for st in RUN {
        let info = with(&text, "SPURINFO").into_iter().find(|l| l.split(' ').nth(1) == Some(st.name())).unwrap();
        assert!(info.ends_with("lost=0"), "{info}");
    }
    let sections: Vec<&str> = with(&text, "REGELN").iter().map(|l| l.split(' ').nth(1).unwrap()).collect();
    for s in ["nbif", "mmhub", "ih", "psp-boot", "psp-ring", "smu", "gc-boot", "gc-hub", "gc-cp", "mes", "gc-ring", "sdma"] {
        assert!(sections.contains(&s), "{s}: {sections:?}");
    }
    // The looks: state between every two stages, alive and bootloader before, alive and state at the end.
    assert_eq!(with(&text, "BLICK").iter().filter(|l| l.starts_with("BLICK state nach=") && l.contains(" writes=0 ")).count(), RUN.len() + 2);
    assert!(with(&text, "BLICK").iter().all(|l| !l.contains(" writes=") || l.contains(" writes=0 ")), "a look wrote");
    assert!(with(&text, "GPU").iter().any(|l| l.starts_with("GPU vorher")) && with(&text, "GPU").iter().any(|l| l.starts_with("GPU nachher")));
    assert!(text.lines().last().unwrap().starts_with("ENDE nach=ringtest"), "{}", text.lines().last().unwrap());
    // The IH counted the copy engines' traps and the rings' end-of-pipe entries.
    assert!(with(&text, "IH").iter().any(|l| l.starts_with("IH sdmatest") && l.contains("taken=sdma")), "{:?}", with(&text, "IH"));
    assert!(with(&text, "IH").iter().any(|l| l.starts_with("IH ringtest") && l.contains("gfx12 end of pipe")), "{:?}", with(&text, "IH"));
    // No stage reached the card deep in the stack (an unoptimised host build's frames are larger than the kernel's).
    for l in with(&text, "STAPEL") {
        let bytes: usize = l.rsplit('=').next().unwrap().parse().unwrap();
        // tmr sends nothing: it never reaches the card.
        assert!((bytes > 0 || l.starts_with("STAPEL tmr ")) && bytes <= STAGE_STACK * 3 / 2 + 8192, "{l}");
    }
    // SetPptLimit (0x32) never went out, and the smu look (questions Linux's start does not send) never ran.
    let m = card.m.as_ref().unwrap();
    assert!(m.messages.iter().all(|(msg, _)| *msg != 0x32), "{:?}", m.messages);
    assert!(card.calls.iter().all(|(s, _)| *s != Stage::Smu && *s != Stage::Destroy));
    assert!(m.refused.is_empty(), "{:?}", m.refused);
    assert!(room.screen.iter().any(|l| l.contains("15 Sekunden")));
    assert!(room.screen.iter().any(|l| l == "Alle Schritte bestanden."), "{:?}", room.screen);
    // After the run only what is read anew: the attach's power line is not shown again as if measured.
    assert!(with(&text, "GPU").iter().all(|l| !(l.starts_with("GPU nachher") && l.contains("power controller: scpm"))), "{text}");
    assert!(text.contains("GPU nachher nicht neu gelesen:") && text.contains("KOPF stapel_budget="));
    // Every look's START is on the disk before it reads, and each START has its close.
    let starts = with(&text, "BLICK").iter().filter(|l| l.ends_with(" START")).count();
    let closes = with(&text, "BLICK").iter().filter(|l| l.contains(" writes=") && l.contains(" us=")).count();
    assert_eq!((starts, closes), (RUN.len() + 2 + 3, RUN.len() + 2 + 3));
}

#[test]
fn an_answer_of_the_card_that_is_not_the_traces_is_recorded_and_the_run_goes_on() {
    let mut card = fake(model());
    // MMVM_L2_BANK_SELECT_RESERVED_CID2's read-back at the end of gmc's flush: read and not used.
    card.spoil = Some(Spoil { stage: Stage::Gmc, byte: 0x1a500 * 4, nth: 2, xor: 1 });
    let (out, text, room) = run(&mut card);
    assert_eq!(out.stopped, None, "{text}");
    let gmc = with(&text, "VERGLEICH").into_iter().find(|l| l.contains(" gmc ")).unwrap();
    assert!(gmc.contains("said=equal") && gmc.ends_with("answers=1"), "{gmc}");
    assert!(with(&text, "WEITER").iter().any(|l| l.starts_with("WEITER gmc 1 Antworten")), "{text}");
    assert_eq!(room.offs, 1);
}

#[test]
fn a_difference_in_the_middle_stops_the_run_there_and_no_later_stage_touches_the_card() {
    let mut card = fake(model());
    // GFXHUB's L2 control read with a bit Linux's card did not have: gchub writes another value than Linux's.
    card.spoil = Some(Spoil { stage: Stage::GcHub, byte: 0x2825 * 4, nth: 1, xor: 0x10 });
    let (out, text, room) = run(&mut card);
    let (at, why) = out.stopped.clone().unwrap();
    assert_eq!(at, "gchub", "{why}");
    assert_eq!(out.ran.last(), Some(&Stage::GcHub));
    assert!(with(&text, "STOPP").iter().any(|l| l.starts_with("STOPP gchub ")), "{text}");
    assert!(text.contains("VERGLEICH gchub section=gc-hub said=differs"), "{text}");
    // Nothing of the stages after it reached the card: not even asked.
    assert_eq!(card.touched(&[Stage::GcCp, Stage::GcRing, Stage::Sdma, Stage::SdmaTest, Stage::RingTest]), 0);
    assert!(card.calls.iter().all(|(s, _)| !matches!(s, Stage::GcCp | Stage::GcRing | Stage::Sdma | Stage::SdmaTest | Stage::RingTest)));
    // After the stop only the looks read; none wrote.
    let after: Vec<&(Stage, u64)> = card.calls.iter().skip_while(|(s, _)| *s != Stage::GcHub).skip(1).collect();
    assert!(after.iter().all(|(s, _)| matches!(s, Stage::Alive | Stage::State)), "{after:?}");
    assert!(text.lines().last().unwrap().starts_with("ENDE nach=gchub"));
    assert_eq!(room.offs, 1, "the power-off comes after a stop too");
    assert!(room.screen.iter().any(|l| l.starts_with("Angehalten beim Schritt 14 von 21: ")), "{:?}", room.screen);
}

#[test]
fn a_missing_firmware_file_stops_in_the_preflight_with_nothing_written_to_the_card() {
    let mut card = fake(model());
    card.stick.0.remove(&stick_path(fwlist::TA));
    let (out, text, room) = run(&mut card);
    assert_eq!(out.stopped.as_ref().map(|s| s.0.as_str()), Some("vorpruefung"));
    assert!(out.ran.is_empty() && card.calls.is_empty(), "{:?}", card.calls);
    assert!(card.m.as_ref().unwrap().writes.is_empty());
    let ta = with(&text, "FIRMWARE").into_iter().find(|l| l.contains("psp_14_0_3_ta")).unwrap();
    assert!(ta.contains(" FEHLT stage=ta "), "{ta}");
    assert!(text.contains("das Update 175 muss den Firmware-Satz der Karte mitbringen"));
    assert!(text.lines().last().unwrap().starts_with("ENDE nach=vorpruefung"));
    assert_eq!(room.offs, 1);
    // The screen says it plainly; the file's name, the stage and the update are the protocol's.
    assert!(room.screen.contains(&String::from(messung::screen("firmware"))), "{:?}", room.screen);
    assert!(room.screen.iter().all(|l| !l.contains("psp_14") && !l.contains("Stufe ta") && !l.contains("175")), "{:?}", room.screen);
}

#[test]
fn a_card_that_is_not_cold_stops_in_the_preflight_after_reads_only() {
    let mut card = fake(warm());
    let (out, text, room) = run(&mut card);
    assert_eq!(out.stopped.as_ref().map(|s| s.0.as_str()), Some("vorpruefung"), "{text}");
    assert!(out.ran.is_empty());
    assert_eq!(card.calls.iter().map(|(s, _)| *s).collect::<Vec<_>>(), [Stage::Alive], "only the look that found it warm");
    assert!(card.m.as_ref().unwrap().writes.is_empty());
    assert!(text.contains("PRUEF kalt FEHLER C2PMSG_81 is") || text.contains("PRUEF kalt FEHLER C2PMSG_81 ist"), "{text}");
    assert_eq!(room.offs, 1);
    assert!(room.screen.contains(&String::from(messung::screen("kalt"))) && room.screen.iter().all(|l| !l.contains("C2PMSG")), "{:?}", room.screen);
}

#[test]
fn a_card_on_another_driver_path_or_not_the_display_stops_in_the_preflight() {
    for (driver, display, what) in [(Some("vfio-pci"), true, "treiber"), (None, true, "treiber"), (Some(crate::rdna4::messcheck::DRIVER), false, "anzeige")] {
        let mut card = fake(model());
        (card.driver, card.display) = (driver, display);
        let (out, text, _) = run(&mut card);
        assert!(card.calls.is_empty(), "{what}");
        assert!(out.stopped.unwrap().1.starts_with(what), "{what}: {text}");
    }
}

#[test]
fn another_card_or_a_stopped_attach_stops_in_the_preflight() {
    let mut card = Fake::new(facts(&CARD_B), model()).expect("card B attaches");
    let (out, text, _) = run(&mut card);
    assert!(card.calls.is_empty() && out.stopped.unwrap().1.starts_with("karte"), "{text}");
    let mut card = fake(model());
    card.attach = Err(String::from("step 3: the blob does not hold"));
    let (out, _, _) = run(&mut card);
    assert!(card.calls.is_empty() && out.stopped.unwrap().1.starts_with("anschluss"));
}

/// The whole run's bound: checked before each stage, the stage that would start is named, the card is left.
#[test]
fn a_run_past_its_time_limit_stops_before_the_next_stage() {
    let mut card = fake(model());
    let mut disk = Paper::default();
    // Every question to the clock moves it 10 s: the bound comes after a few stages.
    let mut room = Room { step: 10_000_000, ..Room::default() };
    let out = messung::go(&mut card, &mut disk, &mut room);
    let text = disk.text.lock().unwrap().clone();
    let (at, why) = out.stopped.clone().unwrap();
    assert!(why.contains("Zeitgrenze") && RUN.iter().any(|s| s.name() == at), "{at}: {why}");
    assert!(!out.ran.iter().any(|s| s.name() == at), "the stage past the bound ran");
    assert!(out.ran.len() < RUN.len() && text.contains(&format!("STOPP {at} ")), "{text}");
    assert_eq!(room.offs, 1);
}

#[test]
fn a_protocol_that_cannot_be_written_touches_nothing_and_still_switches_off() {
    let mut card = fake(model());
    let mut disk = Paper { refuse_open: true, ..Paper::default() };
    let mut room = Room::default();
    let out = messung::go(&mut card, &mut disk, &mut room);
    assert!(out.file.is_none() && card.calls.is_empty() && out.stopped.is_some());
    assert_eq!(room.offs, 1);
}

fn took(d: Done) -> Result<Took, String> {
    Ok(Took { d, text: String::new(), lowest: usize::MAX, us: 0 })
}

fn done(pass: bool, said: Option<Said>, answers: usize, lost: u64) -> Done {
    Done {
        stage: Stage::Toc,
        pass,
        line: String::from("x"),
        lines: std::vec![String::from("stopped: the fence did not come")],
        totals: Default::default(),
        refused: Vec::new(),
        note: None,
        trail: Vec::new(),
        trail_lost: lost,
        seq: said.map(|said| Verdict { section: "psp-ring", said, answers, line: String::from("linuxseq psp-ring: ...") }),
    }
}

/// The stop rule, each refusal on its own: taking one out turns its line red.
#[test]
fn the_stop_rule_stops_at_every_kind_of_doubt_and_only_there() {
    assert_eq!(messung::next(&took(done(true, Some(Said::Equal), 0, 0))), Next::Go);
    assert_eq!(messung::next(&took(done(true, Some(Said::SoFar), 0, 0))), Next::Go);
    assert_eq!(messung::next(&took(done(true, Some(Said::Equal), 3, 0))), Next::GoWithAnswers(3));
    let stops = |r: Result<Took, String>| matches!(messung::next(&r), Next::Stop(..));
    assert!(stops(Err(String::from("another stage call of this card is running"))), "a call refused before its stage");
    assert!(stops(took(done(false, Some(Said::AsFar), 0, 0))), "a stage that stopped");
    assert!(stops(took(done(false, None, 0, 0))), "a stage refused before any access");
    assert!(stops(took(done(true, Some(Said::Differs), 0, 0))), "an access that is not Linux's");
    assert!(stops(took(done(true, Some(Said::NotCompared), 0, 0))), "a stage not compared");
    assert!(stops(took(done(true, None, 0, 0))), "a stage with no comparison line");
    assert!(stops(took(done(true, Some(Said::Equal), 0, 5))), "a trail that lost accesses");
    let mut wrote_outside = done(true, Some(Said::Equal), 0, 0);
    wrote_outside.refused.push(crate::rdna4::guard::Refusal::Doorbell { index_dw: 7 });
    assert!(stops(took(wrote_outside)), "a write the stage's list refused");
    let Next::Stop(_, why) = messung::next(&took(done(false, Some(Said::AsFar), 0, 0))) else { unreachable!() };
    assert_eq!(why, "stopped: the fence did not come");
}

/// The run holds the card's stage calls for its whole length: a `gx_bringup` call in between is refused.
#[test]
fn a_second_call_while_the_run_holds_the_card_is_refused() {
    let key = crate::devs::Key::new(0, 0x7e, 0, 0);
    let held = Claim::take(key).expect("the first hold");
    assert!(Claim::take(key).is_none(), "a second stage call began while the run held the card");
    drop(held);
    assert!(Claim::take(key).is_some(), "the hold did not end with the run");
}

/// The fixtures the evaluator's test reads (host/tests/gx/30-trace-kit.sh): the protocol of the whole run on the
/// fake card and one with a difference injected in gchub, as the program writes them now. Only the stack figures
/// are set to 0 (they depend on the compiler). `BITSOS_MESSUNG_WRITE=1` writes them anew.
#[test]
fn the_evaluators_fixtures_are_what_the_program_writes() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../host/tests/gx/fixtures");
    let plain = |t: &str| t.lines().map(|l| if l.starts_with("STAPEL ") { format!("{} bytes=0", l.rsplit_once(' ').unwrap().0) } else { String::from(l) }).collect::<Vec<_>>().join("\n") + "\n";
    let good = plain(&run(&mut fake(model())).1);
    let mut card = fake(model());
    card.spoil = Some(Spoil { stage: Stage::GcHub, byte: 0x2825 * 4, nth: 1, xor: 0x10 });
    let bad = plain(&run(&mut card).1);
    for (name, text) in [("gpu-messung-fake.txt", good), ("gpu-messung-differs.txt", bad)] {
        let path = root.join(name);
        if std::env::var("BITSOS_MESSUNG_WRITE").as_deref() == Ok("1") {
            std::fs::write(&path, &text).expect("writing a fixture");
        }
        let kept = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(kept == text, "{} is not what the program writes now: BITSOS_MESSUNG_WRITE=1 cargo test -p amdgpu the_evaluators_fixtures", path.display());
    }
}
