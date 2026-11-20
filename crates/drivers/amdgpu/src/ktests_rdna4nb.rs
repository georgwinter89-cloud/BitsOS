//! Wave 42 in the kernel: the stages `nbif`, `gmc` and `ih` in Linux's order on the model (`rdna4::fakenb`), under
//! the kernel's own locks, clock and klog, each held against its section of Linux's trace with only the declared
//! address rules; then entries of the 18.11. trace through the rings and the reader; then the same stages out of
//! order, refused before any access. QEMU has no RX 9070 XT: the model stands in for it.

use crate::attach::Gop;
use crate::devs::Key;
use crate::gpucore::psp::Timeouts;
use crate::rdna4::Card;
use crate::rdna4::fakecard::FakeCard;
use crate::rdna4::fakefiles::{PICTURE, Stick, model};
use crate::rdna4::fakeih::ModelIo;
use crate::rdna4::nbseq;
use crate::rdna4::regs14::Regs14;
use crate::rdna4::stage::{self, Ctx, Opts, Session, SessionSlot, Stage};
use alloc::format;
use ktest::kassert;
use rdna4_core::card::CARD_A;
use rdna4_core::ih7::{Iv, Ring};

const NB: [Stage; 3] = [Stage::Nbif, Stage::Gmc, Stage::Ih];

fn rig() -> Result<(crate::rdna4::attach::PciFacts, crate::rdna4::attach::Attached), &'static str> {
    let (card, _) = Card::attach(crate::ktests_devs::card_facts(Key::new(0, 4, 0, 0)), FakeCard::measured());
    let facts = card.facts;
    card.state.map(|a| (facts, a)).map_err(|_| "the measured card did not attach")
}

fn stages() -> ktest::TestResult {
    let (facts, att) = rig()?;
    let c = Ctx { index: 0, display: true, facts: &facts, att: &att, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
    let regs = Regs14::from(&att.discovery).ok_or("the table does not name the registers")?;
    // The device's own slot type, so the session lives under the lock the live tool and the IH thread take.
    let slot = SessionSlot::new();
    let mut s = slot.0.lock();
    let mut m = model();
    let mut stick = Stick::full();
    for st in NB {
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut stick);
        m = back;
        kassert!(d.pass && d.refused.is_empty() && d.trail_lost == 0, "{st:?}: {} {:?}", d.line, d.lines);
        if let Err(e) = nbseq::equal(&d, &regs, att.vram_bytes) {
            return ktest::fail(&format!("{st:?} is not Linux's sequence: {e}"));
        }
    }
    kassert!(m.refused.is_empty() && m.nb.context0 && m.nb.ih_on == [true, true], "{:?}", m.refused);
    // The card raises three entries of the trace; the reader finds them where the stage put the rings.
    let ih = s.nb.ih.as_mut().ok_or("the IH is not on")?;
    for (client_id, src_id, d0) in [(4u8, 9u8, 6u32), (20, 181, 0), (4, 87, 0x039A_EE10)] {
        if let Err(e) = m.raise(Ring::Ring0, &Iv { client_id, src_id, src_data: [d0, 0, 0, 0], ..Iv::default() }) {
            return ktest::fail(&e);
        }
    }
    let sys = ih.place.sys;
    let passes = ih.reader.process(&mut ModelIo { m: &mut m, sys });
    kassert!(passes[0].entries == 3 && !passes[0].more && ih.reader.unclaimed.len() == 3 && m.nb.rptr[0] == 96 && m.nb.msi_sent == 1, "{}", ih.reader.describe());
    // The secure processor's stages after them share the flush's new place and the one VRAM manager.
    for st in [Stage::Sos, Stage::Ring, Stage::Toc, Stage::Destroy] {
        let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut stick);
        m = back;
        kassert!(d.pass && d.refused.is_empty(), "{st:?}: {} {:?}", d.line, d.lines);
    }
    kassert!(m.refused.is_empty() && s.nb.gart.is_some() && s.nb.ih.is_some() && s.bump.as_ref().is_some_and(|b| b.taken.len() == 2 && b.given.is_empty()), "{:?}", m.refused);
    Ok(())
}
ktest::ktest!("gx.rdna4.nb.stages", Safe, stages);

fn out_of_order() -> ktest::TestResult {
    let (facts, att) = rig()?;
    let c = Ctx { index: 0, display: true, facts: &facts, att: &att, gop: Some(Gop { phys: CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
    for skip in 0..NB.len() - 1 {
        let mut s = Session::default();
        let mut m = model();
        let mut stopped = false;
        for st in NB.iter().enumerate().filter(|(i, _)| *i != skip).map(|(_, st)| *st) {
            let (before, host) = (m.writes.len(), m.nb.host_changes.len());
            let (d, back) = stage::run(&mut s, Opts { stage: st }, &c, m, &mut Stick::full());
            m = back;
            if !d.pass {
                kassert!(d.totals.writes == 0 && m.writes.len() == before && d.trail.is_empty() && m.nb.host_changes.len() == host, "{st:?} touched the card or the machine's side before refusing");
                stopped = true;
                break;
            }
        }
        // What changed on the machine's side is what a passed nbif asked for, nothing of gmc's or ih's.
        kassert!(stopped && m.refused.is_empty() && m.nb.host_changes.iter().all(|c| *c == "AtomicOp requester enable") && m.nb.sys_given.is_empty(), "without {skip}: {:?} {:?}", m.refused, m.nb.host_changes);
    }
    // The second card: every stage of this wave answers with nothing written.
    let other = Ctx { display: false, index: 1, ..c };
    for st in NB {
        let (d, m) = stage::run(&mut Session::default(), Opts { stage: st }, &other, model(), &mut Stick::full());
        kassert!(!d.pass && d.totals.writes == 0 && m.writes.is_empty() && !m.nb.survey_asked, "{st:?}: {}", d.line);
    }
    Ok(())
}
ktest::ktest!("gx.rdna4.nb.out_of_order", Safe, out_of_order);
