//! GP-GPU2 host fixtures: the files and the model of `rdna4::fakefiles`, and card A attached read-only.

use super::rdna4::facts;
use crate::attach::Gop;
use crate::gpucore::psp::{Hw, Timeouts};
use crate::rdna4::host::Host;
use crate::rdna4::Card;
use crate::rdna4::attach::{Attached, PciFacts};
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::stage::{self, Ctx, Done, Opts, Session, Stage};
use rdna4_core::card::CARD_A;
use std::vec::Vec;
pub use crate::rdna4::fakefiles::*;

/// Card A attached read-only, the session's context.
pub struct Rig {
    pub facts: PciFacts,
    pub att: Attached,
    pub display: bool,
    pub inherited: Option<crate::gpucore::handoff::StopNote>,
}

impl Rig {
    pub fn new() -> Rig {
        let (card, _) = Card::attach(facts(&CARD_A), FakeCard::measured());
        Rig { facts: card.facts, att: card.state.expect("the measured card attaches"), display: true, inherited: None }
    }

    pub fn ctx(&self) -> Ctx<'_> {
        Ctx {
            index: 0,
            display: self.display,
            facts: &self.facts,
            att: &self.att,
            gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }),
            inherited: self.inherited,
            t: Timeouts::LINUX,
        }
    }

    /// One stage on a card (the model, or a wrapper around it).
    pub fn run_on<H: Hw + Host>(&self, s: &mut Session, stage: Stage, hw: H, stick: &mut Stick) -> (Done, H) {
        stage::run(s, Opts { stage }, &self.ctx(), hw, stick)
    }

    /// One stage on the model.
    pub fn run(&self, s: &mut Session, stage: Stage, m: Model, stick: &mut Stick) -> (Done, Model) {
        self.run_on(s, stage, m, stick)
    }

    /// These stages in turn, whatever they answer.
    pub fn all(&self, s: &mut Session, mut m: Model, stick: &mut Stick, stages: &[Stage]) -> (Vec<Done>, Model) {
        let mut out = Vec::new();
        for &st in stages {
            let (d, back) = self.run(s, st, m, stick);
            m = back;
            out.push(d);
        }
        (out, m)
    }

    /// These stages in turn, each of which must pass with nothing refused on either side.
    pub fn pass(&self, s: &mut Session, m: Model, stick: &mut Stick, stages: &[Stage]) -> (Vec<Done>, Model) {
        let (done, m) = self.all(s, m, stick, stages);
        for d in &done {
            assert!(d.pass && d.refused.is_empty() && d.trail_lost == 0, "{:?}: {} {:?}", d.stage, d.line, d.lines);
        }
        assert!(m.refused.is_empty(), "{:?}", m.refused);
        (done, m)
    }

    /// The three blocks Linux starts before the secure processor, on this card: they must pass. The model's record
    /// of writes and of HDP flushes starts after them, so a test of the secure processor's stages counts its own.
    pub fn nb(&self, s: &mut Session, m: Model) -> Model {
        let (_, mut m) = self.pass(s, m, &mut Stick::full(), &NB);
        m.writes.clear();
        m.flushes = 0;
        m
    }

    /// As `all`, on a card on which the three blocks before the secure processor were started first (`nb`).
    pub fn cold(&self, s: &mut Session, m: Model, stick: &mut Stick, stages: &[Stage]) -> (Vec<Done>, Model) {
        let m = self.nb(s, m);
        self.all(s, m, stick, stages)
    }

    /// As `pass`, on a card on which the three blocks before the secure processor were started first (`nb`).
    pub fn cold_pass(&self, s: &mut Session, m: Model, stick: &mut Stick, stages: &[Stage]) -> (Vec<Done>, Model) {
        let m = self.nb(s, m);
        self.pass(s, m, stick, stages)
    }

    /// A session and the model after these stages passed, from a cold card on which the three blocks Linux
    /// starts before the secure processor (`NB`) passed first.
    pub fn up_to(&self, stages: &[Stage]) -> (Session, Model) {
        self.up_on(model(), stages)
    }

    /// As `up_to`, on this model.
    pub fn up_on(&self, m: Model, stages: &[Stage]) -> (Session, Model) {
        let mut s = Session::default();
        let (_, m) = self.cold_pass(&mut s, m, &mut Stick::full(), stages);
        (s, m)
    }
}

/// The three blocks Linux starts before the secure processor, in Linux's order.
pub const NB: [Stage; 3] = [Stage::Nbif, Stage::Gmc, Stage::Ih];

/// The secure processor's stages from a cold card to loaded firmware: the trace's sections psp-boot and psp-ring.
pub const ORDER: [Stage; 8] = [Stage::Sos, Stage::Ring, Stage::Toc, Stage::Tmr, Stage::Fw, Stage::Autoload, Stage::FwRest, Stage::Ta];

/// The cold model as Linux's secure-processor block meets it: NBIF, GMC and IH started by their stages (the HDP
/// flush remapped, the GART on, both rings on), nothing sent to the bootloader yet.
pub fn started() -> Model {
    Rig::new().up_to(&[]).1
}
