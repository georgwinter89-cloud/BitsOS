//! Wave 42: where the stages `nbif`, `gmc` and `ih` stop and what a stop leaves: a `nbif` that stopped half-way
//! did not pass and runs again; a machine that grants no AtomicOps or has no memory for the rings is left
//! unchanged; each stage's guard holds the index pair to its own addresses and to Linux's change there; the
//! policy's rule for a card that sends; and an interrupt source nobody handles is logged once, not per entry.

use super::rdna4fix::*;
use super::rdna4nb::{regs, up};
use super::rdna4nbmodel::{after, n};
use crate::gpucore::psp::{Bound, Cond, Hw};
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::fakemodel::Model;
use crate::rdna4::guard::{Allow, Guard, Refusal};
use crate::rdna4::host::Host;
use crate::rdna4::stage::{self, Opts, Session, Stage};
use crate::rdna4::{nb, nbseq, policy};
use rdna4_core::ih7::{Iv, Ring};
use rdna4_core::nbreg;
use rdna4_core::regmap::SMN_MP1_FIRMWARE_FLAGS;
use std::string::String;
use std::vec::Vec;

/// The model with one register whose `nth` write does not arrive (a write lost mid-stage).
struct FailNth {
    m: Model,
    byte: u32,
    nth: u32,
    seen: u32,
}

impl Hw for FailNth {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.m.reg_read(byte)
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        if byte == self.byte {
            self.seen += 1;
            if self.seen == self.nth {
                return false;
            }
        }
        self.m.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.m.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.m.mem_write(mc, data)
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

impl Host for FailNth {
    fn atomics(&mut self) -> Result<String, String> {
        self.m.atomics()
    }
}

#[test]
fn a_nbif_that_stopped_half_way_did_not_pass_and_runs_again() {
    let rig = Rig::new();
    let n = n(&rig);
    let r = regs(&rig);
    let aperture = n.byte(nbreg::DOORBELL_APER_EN).unwrap();
    // The stage stops at each step after the memory flush's remap: the register flush's remap, DF's write (the
    // data register's second write, the strap's is the first) and the doorbell aperture.
    let reg_flush = n.byte(nbreg::REMAP_HDP_REG_FLUSH).unwrap();
    for (byte, nth, both, what) in [(reg_flush, 1, false, "the register flush's remap"), (n.data, 2, true, "DF's write"), (aperture, 1, true, "the doorbell aperture")] {
        let mut s = Session::default();
        let card = FailNth { m: model(), byte, nth, seen: 0 };
        let (d, card) = stage::run(&mut s, Opts { stage: Stage::Nbif }, &rig.ctx(), card, &mut Stick::full());
        assert!(!d.pass, "{what}: {:?}", d.lines);
        assert!(s.nb.remapped && !s.nb.nbif, "{what}: the flush moved, the stage did not pass");
        let mut m = card.m;
        assert_eq!((m.nb.remapped, m.reg_read(aperture)), ([true, both], Some(0)));
        assert!(s.nb.describe().contains("nbif stopped half-way"), "{}", s.nb.describe());
        // gmc and ih refuse, with nothing written and nothing read.
        for st in [Stage::Gmc, Stage::Ih] {
            let writes = m.writes.len();
            let (d, back) = rig.run(&mut s, st, m, &mut Stick::full());
            m = back;
            assert!(!d.pass && d.line.contains(if st == Stage::Gmc { "zuerst den Anschluss der Karte einrichten" } else { "zuerst den Speicher der Karte einrichten" }), "{what}, {st:?}: {}", d.line);
            assert_eq!((d.totals.writes, m.writes.len() - writes, d.trail.len()), (0, 0, 0));
            assert!(st != Stage::Gmc || d.lines.last().unwrap().contains("stopped half-way, run nbif again"), "{:?}", d.lines);
        }
        assert!(m.nb.sys_given.is_empty() && s.nb.plan.is_none() && s.bump.is_none());
        // nbif again: the whole sequence, and now it passes; then the rest as Linux's.
        let (d, mut m) = rig.run(&mut s, Stage::Nbif, m, &mut Stick::full());
        assert!(d.pass && s.nb.nbif, "{what}: {} {:?}", d.line, d.lines);
        assert!(d.lines.iter().any(|l| l.contains("an earlier run stopped after the HDP remap")), "{:?}", d.lines);
        assert_eq!((m.nb.strap2, m.nb.df, m.reg_read(aperture)), (0x0313_2000, 0x0A41_0031, Some(1)));
        assert!(m.refused.is_empty(), "{:?}", m.refused);
        assert_eq!(m.nb.host_changes, ["AtomicOp requester enable"], "set once, found set by the second run");
        for st in [Stage::Gmc, Stage::Ih] {
            let (d, back) = rig.run(&mut s, st, m, &mut Stick::full());
            m = back;
            assert!(d.pass, "{what}, {st:?}: {} {:?}", d.line, d.lines);
            nbseq::equal(&d, &r, rig.att.vram_bytes).unwrap_or_else(|e| panic!("{what}, {st:?}: {e}"));
        }
        // A passed nbif is refused a second time, as before.
        let (d, m) = rig.run(&mut s, Stage::Nbif, m, &mut Stick::full());
        assert!(!d.pass && d.line.contains("schon") && m.refused.is_empty(), "{}", d.line);
    }
}

#[test]
fn a_machine_that_says_no_is_left_as_it_was() {
    let rig = Rig::new();
    // nbif: the way to the root port grants no AtomicOps. No register is written, requester enable stays off.
    let mut s = Session::default();
    let mut m = model();
    m.nb.atomics_path = Err("the switch port 0000:02:00.0 does not route AtomicOps");
    let (d, mut m) = rig.run(&mut s, Stage::Nbif, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("der Weg zum Prozessor lässt eine Einstellung der Karte nicht zu"), "{}", d.line);
    assert!(d.lines.last().unwrap().contains("AtomicOps to the root port: the switch port 0000:02:00.0 does not route AtomicOps"), "{:?}", d.lines);
    assert_eq!((d.totals.writes, m.writes.len(), d.trail.len()), (0, 0, 0));
    assert!(!m.nb.atomics_req && m.nb.host_changes.is_empty() && !s.nb.remapped && !s.nb.nbif && m.refused.is_empty());
    // Granted, the same session passes and is Linux's sequence.
    m.nb.atomics_path = Ok(());
    let (d, m) = rig.run(&mut s, Stage::Nbif, m, &mut Stick::full());
    assert!(d.pass && m.nb.atomics_req, "{:?}", d.lines);
    nbseq::equal(&d, &regs(&rig), rig.att.vram_bytes).unwrap_or_else(|e| panic!("{e}"));
    // ih: no memory for the rings. Asked before the function is enabled, so nothing changed: no bus master
    // switched, no vector, no register write.
    let (mut s, mut m) = after(&rig, &[Stage::Nbif, Stage::Gmc]);
    m.nb.bus_master = false;
    m.nb.pages_refused = true;
    let (writes, host) = (m.writes.len(), m.nb.host_changes.clone());
    let (d, mut m) = rig.run(&mut s, Stage::Ih, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("kein Arbeitsspeicher für die Karte frei"), "{}", d.line);
    assert!(d.lines.last().unwrap().contains("no pages for the IH rings"), "{:?}", d.lines);
    assert_eq!((d.totals.writes, m.writes.len() - writes), (0, 0));
    assert!(m.nb.host_changes == host && !m.nb.bus_master && !m.nb.msi_on && s.nb.ih.is_none() && m.refused.is_empty());
    // The model's PCI core hands out no vector for a function that may not send.
    assert_eq!(m.msi(), Err(String::from("the function may not send")));
    assert!(!m.nb.msi_on);
    // gmc: no page for the fault's default page.
    let (mut s, mut m) = after(&rig, &[Stage::Nbif]);
    m.nb.pages_refused = true;
    let writes = m.writes.len();
    let (d, m) = rig.run(&mut s, Stage::Gmc, m, &mut Stick::full());
    assert!(!d.pass && d.line.contains("kein Arbeitsspeicher für die Karte frei"), "{}", d.line);
    assert_eq!((d.totals.writes, m.writes.len() - writes), (0, 0));
    assert!(!m.nb.context0 && s.nb.gart.is_none());
}

#[test]
fn the_rule_for_a_card_that_sends() {
    assert!(policy::may_send(true, Ok(())).is_ok());
    // The second card never, whatever the survey says.
    assert!(policy::may_send(false, Ok(())).unwrap_err().contains("only the driven card"));
    assert!(policy::may_send(false, Err(String::from("TranslationOn"))).unwrap_err().contains("only the driven card"));
    assert_eq!(policy::may_send(true, Err(String::from("TranslationOn"))), Err(String::from("DMAR survey: TranslationOn")));
    // The stages ask it where they hand the card system memory: its words are the stop's detail.
    let rig = Rig::new();
    for (before, st) in [(&[Stage::Nbif][..], Stage::Gmc), (&[Stage::Nbif, Stage::Gmc][..], Stage::Ih)] {
        let (mut s, mut m) = after(&rig, before);
        m.nb.survey = Err("TranslationOn");
        let (d, _) = rig.run(&mut s, st, m, &mut Stick::full());
        assert!(!d.pass && d.lines.last().unwrap().contains("DMAR survey: TranslationOn"), "{st:?}: {:?}", d.lines);
    }
}

fn why(g: &Guard<Model>) -> &'static str {
    match g.refused.last() {
        Some(Refusal::Reg { why, .. }) => why,
        other => panic!("{other:?}"),
    }
}

#[test]
fn each_stages_guard_holds_the_index_pair_to_its_own_addresses_and_linuxs_change() {
    let rig = Rig::new();
    let n = n(&rig);
    let list = |stage| Allow { stage, regs: std::vec![n.index, n.data], mem: Vec::new() };
    // nbif: its two registers, not the SMU's flags.
    let mut g = Guard::new(model(), list("nbif"), regs(&rig)).with_index_rules(nb::index_rules(Stage::Nbif, &n));
    g.hw.nb.atomics_req = true;
    assert!(!g.reg_write(n.index, SMN_MP1_FIRMWARE_FLAGS) && why(&g).contains("this stage did not review"));
    assert!(!g.reg_write(n.data, 0) && why(&g).contains("written only at an address where Linux changes a register"), "aimed nowhere yet");
    assert!(g.reg_write(n.index, n.strap2));
    assert!(!g.reg_write(n.data, 0x0313_2000) && why(&g).contains("not Linux's change"), "not read yet");
    let v = g.reg_read(n.data).unwrap();
    assert!(!g.reg_write(n.data, v | 1) && !g.reg_write(n.data, 0));
    assert!(g.reg_write(n.data, nbreg::strap2(v)));
    // What was read at the strap licenses nothing at DF's register.
    assert!(g.reg_write(n.index, n.df));
    assert!(!g.reg_write(n.data, nbreg::df_ncs_config1(0x0A40_0001)) && why(&g).contains("not Linux's change"));
    let v = g.reg_read(n.data).unwrap();
    assert!(!g.reg_write(n.data, v) && !g.reg_write(n.data, nbreg::df_ncs_config1(v) | 0x4000_0000));
    assert!(g.reg_write(n.data, nbreg::df_ncs_config1(v)));
    assert_eq!((g.hw.nb.strap2, g.hw.nb.df), (0x0313_2000, 0x0A41_0031));
    assert!(g.hw.refused.is_empty() && g.refused.len() == 8, "none of it reached the card: {:?}", g.hw.refused);
    // smu: the flags, read only; nothing of nbif's.
    let mut g = Guard::new(model(), list("smu"), regs(&rig)).with_index_rules(nb::index_rules(Stage::Smu, &n));
    assert!(!g.reg_write(n.index, n.strap2) && !g.reg_write(n.index, n.df));
    // Nothing is put back into the index, read there before or not: Linux leaves it on its last address.
    assert_eq!(g.reg_read(n.index), Some(0x1234_0000));
    assert!(!g.reg_write(n.index, 0x1234_0000), "what the index held is not written back");
    assert!(g.reg_write(n.index, SMN_MP1_FIRMWARE_FLAGS));
    assert_eq!(g.reg_read(n.data), Some(1));
    assert!(!g.reg_write(n.data, 1) && why(&g).contains("written only at an address where Linux changes a register"));
    assert!(!g.reg_write(n.index, 0x1234_0000));
    // The stage that starts the power controller reaches the same one address, read only.
    let mut g = Guard::new(model(), list("pmfw"), regs(&rig)).with_index_rules(nb::index_rules(Stage::Pmfw, &n));
    assert!(!g.reg_write(n.index, n.strap2) && g.reg_write(n.index, SMN_MP1_FIRMWARE_FLAGS) && !g.reg_write(n.data, 1));
    // Every other stage: no address at all, even with both registers on its list.
    for st in Stage::ALL.into_iter().filter(|s| !matches!(s, Stage::Nbif | Stage::Smu | Stage::Pmfw)) {
        assert!(nb::index_rules(st, &n).is_empty(), "{st:?}");
    }
    let mut g = Guard::new(model(), list("ring"), regs(&rig)).with_index_rules(nb::index_rules(Stage::Ring, &n));
    for target in n.index_targets() {
        assert!(!g.reg_write(n.index, target));
    }
    assert!(!g.reg_write(n.data, 0) && g.hw.writes.is_empty());
}

#[test]
fn a_source_nobody_handles_is_logged_once_and_counted_every_time() {
    let rig = Rig::new();
    let filter = klog::Filter { level: None, subsystem: Some("gx"), name: Some("rdna4.ih.unclaimed") };
    // The host's log ring is shared with the tests running beside this one: a record overwritten before it is
    // read fails one try, so a few tries with a source of their own each.
    let mut exact = false;
    for client in 201u8..209 {
        let mut s = Session::default();
        let (_, mut m) = up(&rig, &mut s, model());
        let ih = s.nb.ih.as_mut().unwrap();
        let sys = ih.place.sys;
        let seq = klog::next_seq();
        // Three entries of one source in one look, then two more in a second look.
        for looks in [3u32, 2] {
            for i in 0..looks {
                m.raise(Ring::Ring0, &Iv { client_id: client, src_id: 202, src_data: [i, 0, 0, 0], ..Iv::default() }).unwrap();
            }
            assert_eq!(ih.reader.process(&mut ModelIo { m: &mut m, sys })[0].entries, looks);
        }
        assert_eq!(ih.reader.unclaimed[&(client, 202)], 5);
        let mine = std::format!("client={client} source=202 ");
        let logged = klog::read(seq, 4096, &filter).records.iter().filter(|e| e.level == klog::Level::Info && e.text.contains(&mine)).count();
        assert!(logged <= 1, "logged {logged} times for one source");
        exact |= logged == 1;
        if exact {
            break;
        }
    }
    assert!(exact, "rdna4.ih.unclaimed was never logged for a new source");
}
