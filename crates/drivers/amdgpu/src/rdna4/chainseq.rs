//! The chain as a whole: Linux's order from a cold card to loaded firmware (wave 42) and on into the graphics
//! block (wave 43: `gcboot`, `gchub`; wave 44: `gccp`, `mes`, `mesq`, `gcring`), `Stage::CHAIN`, held against the register
//! trace of Linux's own start in one go, every section the chain covers (`linuxseq`): which stages make up which
//! section, each section's declared differences, and the comparison over all of them. The host test and the ktest
//! of the whole chain use this one declaration, and so does every stage call itself (`verdict`, asked by
//! `stage::run`): its answer ends with what the comparison found, on the real card as on the fake, where no test
//! stands by.
//!
//! Not covered, and said here so nobody reads the silence as equal: `discovery` (the attach reads the table through
//! BAR0 where Linux reads it through MM_INDEX, and Linux's wait for IFWI there has no rule yet), and `sw-init`
//! between `mmhub` and `ih` (the later blocks' set-up; the chain takes only its TLB flush after a bind, through
//! `Rule::BindBefore`).

use super::linuxseq::compare::{Compare, Diff};
use super::linuxseq::fold::Bus;
use super::linuxseq::rules::Rule;
use super::linuxseq::{Section, section};
use super::regs14::Regs14;
use super::stage::{Done, Stage};
use super::vram::{Bump, Layout};
use super::{gcstage, nbseq, pmfw, stage_pmfw, stage_ring, stage_sos};
use crate::gpucore::trail::Access;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// The trace's sections in Linux's order, each with the stages whose accesses make it up, in order.
pub const COVER: [(&str, &[Stage]); 12] = [
    ("nbif", &[Stage::Nbif]),
    ("mmhub", &[Stage::Gmc]),
    ("ih", &[Stage::Ih]),
    ("psp-boot", &[Stage::Sos]),
    ("psp-ring", &[Stage::Ring, Stage::Toc, Stage::Tmr, Stage::Fw, Stage::Autoload, Stage::FwRest, Stage::Ta]),
    ("smu", &[Stage::Pmfw]),
    ("gc-boot", &[Stage::GcBoot]),
    ("gc-hub", &[Stage::GcHub]),
    ("gc-cp", &[Stage::GcCp]),
    ("mes", &[Stage::Mes, Stage::MesQ]),
    ("gc-ring", &[Stage::GcRing]),
    ("sdma", &[Stage::Sdma]),
];

/// What this boot chose that a section's declared rules stand on. A section whose part is not known yet has no
/// rules, is not compared, and says why.
#[derive(Clone, Copy, Default)]
pub struct Chosen<'a> {
    /// The window `gmc` planned: the secure processor's sections name addresses inside it.
    pub l: Option<&'a Layout>,
    /// The card's VRAM manager of this boot, whose record says where a section's own buffers lie (the PMFW's two
    /// tables).
    pub taken: Option<&'a Bump>,
    /// What `gmc` told MMHUB, which `gchub` must tell GFXHUB.
    pub told: Option<&'a gcstage::Told>,
    /// The registers Linux's interrupt handlers reach (BAR5 bytes): a stage that reads the IH rings itself makes
    /// those accesses when an entry asks for them, and they are no part of its own sequence (`sdmaseq`).
    pub irq: &'a [u32],
    /// Where the stage `mes` bound the KIQ pipe's queue (`messtage::State::kiq`).
    pub mes: Option<super::messeq::Kiq>,
    /// Where the stage `sdma` bound the engines' rings and slots (`sdmastage::State::bound`).
    pub sdma: Option<super::sdmaseq::Bound>,
}

/// The declared differences of a section: addresses BitsOS chooses itself, the bind before the IH rings and the
/// copy engines' rings are named, the flush before the bootloader's buffer is named, the autoload's loop; or why
/// the section has none yet.
pub fn rules(name: &str, vram_bytes: u64, c: Chosen) -> Result<Vec<Rule>, &'static str> {
    let window = c.l.ok_or("no window is planned, so the section's addresses have no rule");
    match name {
        "psp-boot" => window.map(|l| stage_sos::linux_rules(l).to_vec()),
        "psp-ring" => window.map(|l| stage_ring::linux_rules(l).to_vec()),
        "smu" => {
            let l = window?;
            let placed = c.taken.and_then(|b| pmfw::placed(l, b)).ok_or("the VRAM manager's record does not hold what the section names, so the section's addresses have no rule")?;
            Ok(stage_pmfw::linux_rules(l, placed).to_vec())
        }
        "gc-boot" | "gc-hub" => gcstage::linux_rules(name, c.told).ok_or("no GART is on record, so what GFXHUB must be told is not known"),
        "gc-cp" => {
            let l = window?;
            c.taken.and_then(|b| super::cpseq::placed(l, b)).map(super::cpseq::linux_rules).ok_or("the VRAM manager's record holds no clear-state buffer, so the section's address has no rule")
        }
        "mes" => super::messeq::rules(c.mes),
        // No address BitsOS chooses reaches a register of the section (`ringseq`).
        "gc-ring" => Ok(Vec::new()),
        "sdma" => Ok(super::sdmaseq::rules(c.sdma)),
        _ => Ok(COVER.iter().find(|(n, _)| *n == name).map(|(_, stages)| nbseq::rules(stages[0], vram_bytes)).unwrap_or_default()),
    }
}

/// What was compared: per section its name, how many of Linux's lines it holds and how many accesses of the chain
/// went against them.
pub type Compared = Vec<(&'static str, usize, usize)>;

/// The chain's answers, one per stage of `Stage::CHAIN` in order, against every covered section. `Ok` or the first
/// difference in plain words with its section and stage.
pub fn equal(done: &[Done], regs: &Regs14, l: &Layout, taken: Option<&Bump>, mes: Option<super::messeq::Kiq>, sdma: Option<super::sdmaseq::Bound>) -> Result<Compared, String> {
    let ran: Vec<Stage> = done.iter().map(|d| d.stage).collect();
    if ran != Stage::CHAIN {
        return Err(format!("not Linux's chain: {ran:?}"));
    }
    if let Some(d) = done.iter().find(|d| !d.pass || d.trail_lost != 0) {
        return Err(format!("{}: {} ({} accesses lost)", d.stage.name(), d.line, d.trail_lost));
    }
    let bus = Bus::of(regs);
    let mut compared = Vec::new();
    let mut at = 0;
    // What MMHUB was told, as the bus saw it in `gmc`'s trail: GFXHUB's rules ask for exactly these addresses.
    let mut told = None;
    for (name, stages) in COVER {
        let sec = section(name)?;
        let chosen = Chosen { l: Some(l), taken, told: told.as_ref(), irq: &[], mes, sdma };
        let rules = self::rules(name, l.vram_bytes, chosen).map_err(|e| format!("{name}: {e}"))?;
        let mut c = Compare::new(&sec, &rules).map_err(|e| format!("{name}: {e}"))?;
        let mut accesses = 0;
        for d in &done[at..at + stages.len()] {
            c.feed_trail(&d.trail, d.trail_lost, &bus).map_err(|e| format!("{name}, stage {}: {e}", d.stage.name()))?;
            accesses += d.trail.len();
        }
        told = told.or(gcstage::Told::seen(name, c.addresses()));
        c.finish().map_err(|e| format!("{name}: {e}"))?;
        compared.push((name, sec.lines.len(), accesses));
        at += stages.len();
    }
    Ok(compared)
}

/// What the card saw of the section whose stages run now, kept across the stage calls of one boot
/// (`Session::seq`): a section of several stages is compared from its start with each of them.
#[derive(Default)]
pub struct Seen {
    open: Option<Open>,
}

struct Open {
    section: &'static str,
    /// The accesses of the section's stages that passed so far, in order, and what their trails lost.
    trail: Vec<Access>,
    lost: u64,
    /// Accesses of calls that stopped: the card saw them too, and they are in no comparison after their own.
    stopped: usize,
}

impl Seen {
    pub const fn new() -> Seen {
        Seen { open: None }
    }
}

/// The answers that differ from the trace's and the line of init.txt Linux goes on at, or the first difference.
/// `whole`: the section must be used up.
fn held(sec: &Section, rules: &[Rule], trail: &[Access], lost: u64, bus: &Bus, whole: bool) -> Result<(Vec<Diff>, Option<u32>), Diff> {
    let mut c = Compare::new(sec, rules)?.answers_kept();
    c.feed_trail(trail, lost, bus)?;
    let (answers, next) = (c.answers().to_vec(), c.next_line());
    if whole {
        c.finish()?;
    }
    Ok((answers, next))
}

/// One stage call held against Linux's trace, as one line of the stage's answer: `linuxseq <section>: equal`,
/// `equal so far` while later stages of the section are still to come, or `differs:` with the first difference in
/// plain words. `None` for a look, the destroy and a call that was refused before any access.
///
/// What the stage does must be Linux's, access for access. What the card answers is the card's: an answer that
/// is not the trace's is counted and named, and the accesses after it are compared on (on the real card the
/// hardware round reads both from this line; on the fake there are none). A call that stopped is compared as far
/// as it came and its accesses are not carried into the next comparison, which says how many there were.
pub fn verdict(seen: &mut Seen, stage: Stage, pass: bool, trail: &[Access], lost: u64, regs: &Regs14, vram_bytes: u64, chosen: Chosen) -> Option<String> {
    judge(seen, stage, pass, trail, lost, regs, vram_bytes, chosen).map(|v| v.line)
}

/// What a stage's `linuxseq` line says, as a kind: the measurement run's stop rule keys on it, not on the words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Said {
    /// `equal`: the section is used up and every access was Linux's.
    Equal,
    /// `equal so far`: Linux's section goes on in a later stage.
    SoFar,
    /// The stage stopped; as far as it came its accesses were Linux's.
    AsFar,
    /// An access that is not Linux's: the driver's own doing.
    Differs,
    /// No comparison was possible: nothing proves the call.
    NotCompared,
}

impl Said {
    pub fn word(self) -> &'static str {
        match self {
            Said::Equal => "equal",
            Said::SoFar => "so-far",
            Said::AsFar => "as-far",
            Said::Differs => "differs",
            Said::NotCompared => "not-compared",
        }
    }
}

/// One stage call's `linuxseq` line with what it says: the section, the kind, how many answers of the card were
/// not the trace's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub section: &'static str,
    pub said: Said,
    pub answers: usize,
    pub line: String,
}

/// `verdict`'s typed twin: the same line with its kind.
pub fn judge(seen: &mut Seen, stage: Stage, pass: bool, trail: &[Access], lost: u64, regs: &Regs14, vram_bytes: u64, chosen: Chosen) -> Option<Verdict> {
    if stage == Stage::SdmaTest {
        // Outside the chain and in no section: held against the one unit of the trace it repeats (`sdmaseq`).
        return super::sdmaseq::rungs_judge(pass, trail, lost, regs, chosen.irq);
    }
    if stage == Stage::RingTest {
        return super::ringseq::rungs_judge(pass, trail, lost, regs, chosen.irq);
    }
    let (name, stages) = COVER.into_iter().find(|(_, st)| st.contains(&stage))?;
    if !pass && trail.is_empty() && lost == 0 {
        return None;
    }
    let (first, last) = (stages.first() == Some(&stage), stages.last() == Some(&stage));
    let rules = match self::rules(name, vram_bytes, chosen) {
        Ok(rules) => rules,
        Err(why) => return Some(Verdict { section: name, said: Said::NotCompared, answers: 0, line: format!("linuxseq {name}: not compared: {why}") }),
    };
    let sec = match section(name) {
        Ok(sec) => sec,
        Err(e) => return Some(Verdict { section: name, said: Said::NotCompared, answers: 0, line: format!("linuxseq {name}: not compared: {e}") }),
    };
    let mut open = match seen.open.take() {
        Some(o) if o.section == name => o,
        _ if first => Open { section: name, trail: Vec::new(), lost: 0, stopped: 0 },
        other => {
            seen.open = other;
            return Some(Verdict { section: name, said: Said::NotCompared, answers: 0, line: format!("linuxseq {name}: not compared: what the section's stages before {} did is not on record in this boot", stage.name()) });
        }
    };
    let before = if open.stopped == 0 { String::new() } else { format!("; {} accesses of calls that stopped came before it and are not compared", open.stopped) };
    let mut all = open.trail.clone();
    all.extend_from_slice(trail);
    let all_lost = open.lost + lost;
    let found = held(&sec, &rules, &all, all_lost, &Bus::of(regs), pass && last);
    if pass {
        (open.trail, open.lost) = (all, all_lost);
    } else {
        open.stopped += trail.len();
    }
    if !(pass && last) {
        seen.open = Some(open);
    }
    Some(match found {
        Ok((answers, next)) => {
            let (scope, said) = match (pass, last) {
                (false, _) => ("the same as Linux's as far as the stage came", Said::AsFar),
                (true, true) => ("equal", Said::Equal),
                (true, false) => ("equal so far", Said::SoFar),
            };
            let on = next.filter(|_| !(pass && last)).map_or(String::new(), |n| format!(" (Linux goes on at init.txt line {n})"));
            let card = answers.first().map_or(String::new(), |a| format!("; {} answers of the card are not the trace's, the first: {a}", answers.len()));
            Verdict { section: name, said, answers: answers.len(), line: format!("linuxseq {name}: {scope}{on}{card}{before}") }
        }
        Err(e) => {
            let e = format!("{e}");
            klog::event!(Warn, "gx", "rdna4.linuxseq.differs", stage = stage.name(), section = name, what = e.as_str(); "a stage's accesses are not Linux's");
            Verdict { section: name, said: Said::Differs, answers: 0, line: format!("linuxseq {name}: differs: {e}{before}") }
        }
    })
}
