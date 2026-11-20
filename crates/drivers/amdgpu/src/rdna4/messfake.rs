//! The measurement run on the fake card (`messung::Card` over `fakemodel::Model`), for the host tests and the
//! ktests: every stage through `stage::run` as the real card's glue calls it, the answer worded as `gx_bringup`
//! words it, and a probe below the stage's owner that counts what reached the card and can make the card answer
//! one read differently (a difference injected in the middle of the run, `Spoil`).

use super::Card as Attach;
use super::attach::{Attached, PciFacts};
use super::fakecard::FakeCard;
use super::fakefiles::{PICTURE, Stick};
use super::fakemodel::Model;
use super::host::Host;
use super::livestage::{Ran, answer};
use super::messcheck::{self, Facts};
use super::messung::{Card, Took};
use super::regs14::Regs14;
use super::session::Session;
use super::stage::{self, Ctx, Opts, Stage};
use crate::attach::Gop;
use crate::gpucore::psp::{Bound, Cond, Hw, Timeouts};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One read the card answers otherwise: in `stage`, the `nth` read (from 1) of BAR5 byte `byte`, xor `xor`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spoil {
    pub stage: Stage,
    pub byte: u32,
    pub nth: u32,
    pub xor: u32,
}

/// The model with a count of what reached it.
pub struct Probe {
    pub m: Model,
    pub spoil: Option<Spoil>,
    reads_of: u32,
    /// Register reads and writes, memory accesses and doorbells that reached the card in this call.
    pub touched: u64,
}

impl Hw for Probe {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.touched += 1;
        let v = self.m.reg_read(byte);
        match self.spoil {
            Some(s) if s.byte == byte => {
                self.reads_of += 1;
                if self.reads_of == s.nth { v.map(|v| v ^ s.xor) } else { v }
            }
            _ => v,
        }
    }
    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        self.touched += 1;
        self.m.reg_write(byte, value)
    }
    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.touched += 1;
        self.m.mem_read(mc, buf)
    }
    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        self.touched += 1;
        self.m.mem_write(mc, data)
    }
    fn now_us(&mut self) -> u64 {
        self.m.now_us()
    }
    fn delay_us(&mut self, us: u64) {
        self.m.delay_us(us)
    }
    fn flush_hdp(&mut self) {
        self.m.flush_hdp()
    }
    fn invalidate_hdp(&mut self) {
        self.m.invalidate_hdp()
    }
    fn mem_write64(&mut self, mc: u64, value: u64) -> bool {
        self.touched += 1;
        self.m.mem_write64(mc, value)
    }
    fn store_barrier(&mut self) {
        self.m.store_barrier()
    }
    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        self.touched += 1;
        self.m.doorbell_write64(index_dw, value)
    }
    fn doorbell_write32(&mut self, index_dw: u32, value: u32) -> bool {
        self.touched += 1;
        self.m.doorbell_write32(index_dw, value)
    }
    fn wait_begin(&mut self, byte: u32, cond: Cond, bound: Bound) {
        self.m.wait_begin(byte, cond, bound)
    }
    fn wait_end(&mut self) {
        self.m.wait_end()
    }
}

impl Host for Probe {
    fn dma_survey(&mut self) -> Result<(), String> {
        self.m.dma_survey()
    }
    fn bus_master(&mut self) -> Option<bool> {
        self.m.bus_master()
    }
    fn bridges_forward(&mut self) -> Result<(), String> {
        self.m.bridges_forward()
    }
    fn atomics(&mut self) -> Result<String, String> {
        self.m.atomics()
    }
    fn enable_dma(&mut self) -> Result<String, String> {
        self.m.enable_dma()
    }
    fn msi(&mut self) -> Result<String, String> {
        self.m.msi()
    }
    fn pages(&mut self, bytes: u64) -> Option<u64> {
        self.m.pages(bytes)
    }
    fn sys_read(&mut self, at: u64, buf: &mut [u8]) -> bool {
        self.m.sys_read(at, buf)
    }
    fn sys_write(&mut self, at: u64, data: &[u8]) -> bool {
        self.m.sys_write(at, data)
    }
}

/// The fake card the run drives.
pub struct Fake {
    pub facts: PciFacts,
    pub att: Attached,
    pub s: Session,
    /// The model between two calls (inside a call it is the probe's).
    pub m: Option<Model>,
    pub stick: Stick,
    /// The PCI core's binding as the preflight reads it.
    pub driver: Option<&'static str>,
    pub display: bool,
    /// Tests: the read-only attach as the preflight is told it went.
    pub attach: Result<(), String>,
    pub spoil: Option<Spoil>,
    /// Tests: this stage never returns (a hang the protocol must outlive). Only host tests set it: it panics.
    pub hang_at: Option<Stage>,
    /// Tests: the `n`th call (from 1, looks counted) never returns, wherever it falls.
    pub hang_call: Option<usize>,
    /// Every call in order: the stage, and how many accesses reached the card.
    pub calls: Vec<(Stage, u64)>,
}

impl Fake {
    /// The measured card attached read-only, cold, the fixture files on the stick.
    pub fn new(facts: PciFacts, m: Model) -> Option<Fake> {
        let (card, _) = Attach::attach(facts, FakeCard::measured());
        let att = card.state.ok()?;
        Some(Fake { facts: card.facts, att, s: Session::default(), m: Some(m), stick: Stick::full(), driver: Some(messcheck::DRIVER), display: true, attach: Ok(()), spoil: None, hang_at: None, hang_call: None, calls: Vec::new() })
    }

    /// Accesses that reached the card in calls of these stages.
    pub fn touched(&self, stages: &[Stage]) -> u64 {
        self.calls.iter().filter(|(s, _)| stages.contains(s)).map(|(_, n)| n).sum()
    }
}

impl Card for Fake {
    fn facts(&mut self) -> Facts {
        let files = messcheck::files(self.facts.device, self.facts.revision, &mut self.stick);
        Facts {
            key: self.facts.key,
            vendor: self.facts.vendor,
            device: self.facts.device,
            revision: self.facts.revision,
            display: self.display,
            driver: self.driver,
            attach: self.attach.clone(),
            link: format!("{:?}", self.facts.path),
            files,
            rev_id: self.att.rev_id,
        }
    }

    fn stage(&mut self, st: Stage) -> Result<Took, String> {
        if self.hang_at == Some(st) || self.hang_call == Some(self.calls.len() + 1) {
            panic!("the fake card hangs in {}", st.name());
        }
        let mut m = self.m.take().ok_or("the model was lost in an earlier call")?;
        m.deepest = usize::MAX;
        let probe = Probe { m, spoil: self.spoil.filter(|s| s.stage == st), reads_of: 0, touched: 0 };
        let c = Ctx { index: 0, display: self.display, facts: &self.facts, att: &self.att, gop: Some(Gop { phys: rdna4_core::card::CARD_A.bar0_base, bytes: PICTURE }), inherited: None, t: Timeouts::LINUX };
        let mut probe = probe;
        let t0 = probe.now_us();
        let (d, mut probe) = stage::run(&mut self.s, Opts { stage: st }, &c, probe, &mut self.stick);
        let us = probe.now_us().saturating_sub(t0);
        self.calls.push((st, probe.touched));
        let lowest = probe.m.deepest;
        self.m = Some(probe.m);
        let ran = Ran { d, changed: Vec::new(), reader: None, note_lost: false, lowest, us };
        let text = answer("card0", st, &ran);
        Ok(Took { d: ran.d, text, lowest, us })
    }

    fn snapshot(&mut self) -> String {
        let files = super::fw::on_stick(&super::fw::wanted(self.facts.device, self.facts.revision), &mut self.stick);
        // The real snapshot's lines the evaluator reads (`snapshot::text`), from the same facts.
        let mut out = format!("{}\n", super::snapshot::power_line(&self.facts, &self.att));
        out.push_str(&super::fw::lines(&files).join("\n"));
        out.push_str(&format!("\nstages: {}\n", self.s.summary()));
        out
    }

    fn regs(&mut self) -> Option<(Regs14, u64)> {
        Regs14::from(&self.att.discovery).map(|r| (r, self.att.vram_bytes))
    }

    fn session(&mut self, f: &mut dyn FnMut(&Session)) {
        f(&self.s)
    }

    // As `liveih::thermal_look_run` on the card: the taker's read through its gate, the model as the card.
    fn thermal(&mut self) -> super::thermal::Look {
        let (Some(m), Some(ih)) = (self.m.as_mut(), self.s.nb.ih.as_mut()) else { return super::thermal::Look::none(Vec::new(), "no IH") };
        let now = m.card.now_us;
        ih.reader.thermal.look(m, now)
    }
}
