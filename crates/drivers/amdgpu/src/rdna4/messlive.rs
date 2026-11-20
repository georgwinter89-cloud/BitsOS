//! The measurement run on the real card (`messung::Card` over the device): every stage through
//! `livestage::run_one`, the one path `gx_bringup` takes, with the run holding the card's stage calls from its
//! first look to its last (`livestage::Claim`), so no MCP call slips a stage in between.

use super::livestage::{Claim, answer, run_one};
use super::messcheck::{self, Facts};
use super::messung::{Card, Took};
use super::regs14::Regs14;
use super::session::Session;
use super::stage::Stage;
use crate::devs::{self, Device, Family};
use crate::gpucore::fwload::StickSource;
use alloc::format;
use alloc::string::String;

/// What the screen says on a machine without the card (the Mini-PC): one line, nothing touched.
pub const NONE_HERE: &str = "Diese Messung gilt der Grafikkarte RX 9070 XT des Gaming-PC; in diesem PC steckt sie nicht. Es wurde nichts angefasst.";

/// The card the run is for: the RX 9070 XT (1002:7550) that shows the picture, or else the first one.
pub fn find() -> Result<&'static Device, &'static str> {
    let reg = devs::registry();
    let ours = |d: &&'static Device| d.family == Family::Rdna4 && d.pci.as_ref().is_some_and(|f| (f.vendor, f.device) == (rdna4_core::card::VENDOR, rdna4_core::card::DEVICE));
    reg.all().iter().copied().filter(ours).find(|d| d.display).or_else(|| reg.all().iter().copied().find(ours)).ok_or(NONE_HERE)
}

pub struct Live {
    dev: &'static Device,
    claim: Claim,
}

impl Live {
    /// Holds the card's stage calls for the run; refused while another stage call runs.
    pub fn new(dev: &'static Device) -> Result<Live, String> {
        let claim = Claim::take(dev.key).ok_or_else(|| format!("{}: gerade läuft ein anderer Aufruf einer Stufe dieser Karte; bitte später noch einmal", dev.card_name()))?;
        Ok(Live { dev, claim })
    }
}

impl Card for Live {
    fn facts(&mut self) -> Facts {
        let dev = self.dev;
        let card = super::live::card(dev);
        let loc = pci::Loc { segment: dev.key.segment, bus: dev.key.bus, dev: dev.key.dev, func: dev.key.func };
        let (vendor, device, revision, link) = card.map_or((0, 0, 0, String::from("-")), |c| (c.facts.vendor, c.facts.device, c.facts.revision, format!("{:?}, host {:?}", c.facts.path, c.facts.link_host)));
        let attach = match card.map(|c| &c.state) {
            Some(Ok(_)) => Ok(()),
            Some(Err(stop)) => Err(format!("{stop}")),
            None => Err(String::from("no PCI function")),
        };
        let files = messcheck::files(device, revision, &mut StickSource);
        let rev_id = card.and_then(|c| c.state.as_ref().ok()).and_then(|a| a.rev_id);
        Facts { key: dev.key, vendor, device, revision, display: dev.display, driver: pci::bind::bound_driver(loc), attach, link, files, rev_id }
    }

    fn stage(&mut self, st: Stage) -> Result<Took, String> {
        let ran = run_one(self.dev, st, &self.claim)?;
        let text = answer(&self.dev.card_name(), st, &ran);
        Ok(Took { lowest: ran.lowest, us: ran.us, text, d: ran.d })
    }

    fn snapshot(&mut self) -> String {
        super::live::answer(self.dev).unwrap_or_else(|e| e)
    }

    fn regs(&mut self) -> Option<(Regs14, u64)> {
        let card = super::live::card(self.dev)?;
        let att = card.state.as_ref().ok()?;
        Regs14::from(&att.discovery).map(|r| (r, att.vram_bytes))
    }

    fn session(&mut self, f: &mut dyn FnMut(&Session)) {
        let s = self.dev.stages.0.lock();
        f(&s);
    }

    fn thermal(&mut self) -> super::thermal::Look {
        super::liveih::thermal_look_run(self.dev)
    }
}
