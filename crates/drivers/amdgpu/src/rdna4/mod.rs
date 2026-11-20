//! GP-GPU1: the Radeon RX 9070 XT (Navi 48, 1002:7550) attached READ-ONLY, one object per PCI function
//! (plan/spec/gpu-rdna4.md pieces 1, 2, 4, 6, 15, 22-GP-GPU1.md). The Gaming-PC has two cards
//! with the same ids, the same table and the same register bases, so nothing here is keyed by anything but the PCI
//! address (`devs::Key`).
//!
//! What this wave does to a card: read its PCI configuration, read two fixed BAR5 dwords (firmware ready, memory
//! size; three more, the mailbox, only when the default place holds no table) and read its discovery table (10 KiB,
//! at most 64 KiB at a mailbox place) through a small window of BAR0. It writes nothing: no
//! register, no memory, no PCI configuration, no bus master, no power or reset command, on the card with the picture
//! and on the other one alike. The proof is `policy` (the rules and a source check), the register owner with its
//! gate shut (`gpucore::bringup`, as GX-7b's first attach of the 780M) and `fakecard`, which fails on any write.
//!
//! - `policy`: what may and may not be done (item 22), the DMA ceiling recorded for later;
//! - `attach`: the steps, bounded, each failure a named stop;
//! - `snapshot`: the answer the remote desk reads (MCP `gpu`), the lines of the report page;
//! - `fw`: the card's firmware set (names, kicker rule, one MES file) and what of it lies on the stick;
//! - `fakecard`: a card built from the measured table (host tests and ktests);
//! - `live`: the kernel glue (PCI match, BAR5 and BAR0 windows).
//!
//! GP-GPU2 (plan/spec/gpu-rdna4.md section 5) adds the first writes, on the display card only and only when a stage is
//! asked for by MCP: `stage` (what each stage does and the run of one), `stages` (their names, Linux's chain of them
//! and the one check of its order), `ring14` (the secure processor's command ring), `smu14`
//! (the power controller, read-only questions), `fwlist` (what the secure processor gets, in Linux's order), `vram`
//! (where its buffers go), `guard` (each stage's write list), `regs14` (the registers by the card's table),
//! `livestage` (the kernel glue that writes), `fakemodel` (a card that refuses what Linux would not do) with
//! `fakehw` (its bus, where each block's model answers for its own registers).
//!
//! Wave 42 (pieces 6, 10, 11): the three blocks Linux starts before the secure processor. `nb` (the stages' shared
//! state, write lists and bus forms), `nbifstage`, `gmcstage`, `ihstage` (the stages), `nbregs` (their registers
//! by the card's table), `gart12` (the GART and its TLB flush), `ihring` (the rings as the CPU reads them), `host`
//! (the machine's side of a card: the DMAR survey, AtomicOp requester enable, bus master, the MSI vector, system
//! pages), `hostprep` (the chain of decisions that changes the PCI side on the card, before a stage, over `livepci`), `ihoff`
//! (the IH switched off as Linux does, for a Reload), `fakenb` (the model's faces of all of it, with `fakehost`,
//! `fakegart` and `fakeih`; `fakechain` holds the model's rules between these blocks and the secure processor).
//!
//! GX-10 PS (pieces 7 and 8): the card from cold to loaded firmware. `stage_sos` (Linux's nine bootloader loads,
//! opened one step at a time by `rdna4_core::bootseq`), `stage_ring` and `stage_fw` (the ring and its 31 commands
//! of Linux's init), `stage_ro` (the looks outside the chain), `session` (what a boot's stages did so far), `pspfiles` (the secure processor's two files by their
//! headers), `fakeboot` and `fakecmd` (the model's bootloader and its secure OS).
//!
//! Wave 43 (piece 9 beyond read-only): the power controller as Linux starts it after the firmware load.
//! `stage_pmfw` (the stage `pmfw`: Linux's 125 messages of `smu_hw_init`, the PMFW's table read out of VRAM, the
//! thermal alert), `smu14` (the mailbox in Linux's bus form, also the look `smu`'s), `pmfw` (what the start
//! learned, kept in the session: the clocks' levels, whether its features are on), `fakepmfw` and `fakepmfwregs` (the
//! model's PMFW); the sequence, its gate and the table's layout are `rdna4_core::{smuseq, smugate, pptable14}`.
//! Wave 43 (piece 12): SDMA 7.0.1, the copy engine. `sdmaregs` (its registers by the card's table), `sdmaq` (a
//! queue as the CPU drives it: ring, slots, the frame around an IB, the doorbell), `sdmastage` (the stage `sdma`,
//! Linux's hw_init with both ring tests), `sdmarungs` (the stage `sdmatest`: Linux's IB test, then fill, copy,
//! trap and system pages), `sdmaih` (the IH rings read from inside a stage), `sdmaseq` (the declared rules of the
//! trace's section), `sdmaoff` (the engines stopped as Linux stops them, for a Reload), `fakesdma` and
//! `fakesdmaeng` (the model's engines: their registers and what they refuse, and the ring executed through the
//! GART).
//! Wave 44 (TH): the card's thermal interrupts. `thermal` (the taker at the IH reader: Linux's handler of THM's
//! two thresholds and MP1's interrupt, the software CTF check 50 ms after a rising alert, the power-off it can
//! decide), `thermalhw` (its bus accesses, each through `rdna4_core::thmgate`), `fakethermal` (the model's thermal
//! block: the alerts it raises, the temperatures in its metrics table, what it refuses).
//!
//! GX-10 (plan/spec/gpu-rdna4.md section 6): `linuxseq` holds Linux's own start of the card, cut from its register
//! trace, and compares a stage's accesses with it. `chainseq` holds the whole chain (`Stage::CHAIN`: nbif, gmc,
//! ih, sos, ring, toc, tmr, fw, autoload, fwrest, ta, and since wave 43 pmfw, gcboot, gchub, sdma) against every
//! section it covers in one go. What holds between wave 43's items (one power answer, GFXHUB under the copy
//! engines, the flush of both hubs, a second `fw`) is said in the spec's section 6, "The whole chain".

pub mod attach;
pub mod bootnote;
pub mod chainseq;
pub mod cpoff;
pub mod cpring;
pub mod cpseq;
pub mod cpstage;
pub mod depth;
pub mod fakeboot;
pub mod fakecard;
pub mod fakechain;
pub mod fakecmd;
pub mod fakefiles;
pub mod fakegart;
pub mod fakehw;
pub mod fakecp;
pub mod fakecpeng;
pub mod fakegc;
pub mod fakegchub;
pub mod fakehost;
pub mod fakeih;
pub mod fakemodel;
pub mod fakenb;
pub mod fakepmfw;
pub mod fakepmfwregs;
pub mod fakesdma;
pub mod fakesdmaeng;
pub mod fakemes;
pub mod fakemesapi;
pub mod fakemeshqd;
pub mod fakering;
pub mod fakesmu;
pub mod fakethermal;
pub mod fw;
pub mod fwlist;
pub mod gart12;
pub mod gchuboff;
pub mod gcstage;
pub mod gmcstage;
pub mod guard;
pub mod host;
pub mod hostprep;
pub mod ihfault;
pub mod ihoff;
pub mod ihring;
pub mod ihstage;
pub mod linuxseq;
pub mod live;
pub mod livecp;
pub mod livegc;
pub mod liveih;
pub mod livemes;
pub mod livepci;
pub mod livering;
pub mod livesdma;
pub mod livestage;
pub mod mescmd;
pub mod mesflush;
pub mod meskq;
pub mod mesoff;
pub mod mesring;
pub mod messcheck;
pub mod messfake;
pub mod messlive;
pub mod messproto;
pub mod messrun;
pub mod messeq;
pub mod messtage;
pub mod messtherm;
pub mod messung;
pub mod nb;
pub mod nbifstage;
pub mod nbregs;
pub mod nbseq;
pub mod pmfw;
pub mod policy;
pub mod pspfiles;
pub mod regs14;
pub mod ring14;
pub mod ringih;
pub mod ringoff;
pub mod ringops;
pub mod ringrungs;
pub mod ringseq;
pub mod ringstage;
pub mod sdmaih;
pub mod sdmaoff;
pub mod sdmaq;
pub mod sdmaregs;
pub mod sdmarungs;
pub mod sdmaseq;
pub mod sdmastage;
pub mod session;
pub mod smu14;
pub mod snapshot;
pub mod stage;
pub mod stages;
pub mod stage_fw;
pub mod stage_pmfw;
pub mod stage_ring;
pub mod stage_ro;
pub mod stage_sos;
pub mod thermal;
pub mod thermalhw;
pub mod thermallook;
mod stage_util;
pub mod vram;

use crate::gpucore::bringup::{Bringup, Totals};
use crate::gpucore::psp::Hw;
use ksync::{Mutex, Once};

/// One card after its read-only attach: what configuration space said, how far the attach came, and what the
/// register owner counted (no write may ever be counted here).
#[derive(Debug)]
pub struct Card {
    pub facts: attach::PciFacts,
    pub state: Result<attach::Attached, attach::Stop>,
    /// The owner's totals after the attach: every field must be 0.
    pub totals: Totals,
    pub took_us: u64,
}

impl Card {
    /// The attach over `hw`, behind a register owner whose gate is shut (`policy::GATE`): a write, were one tried,
    /// would be refused before the hardware and counted.
    pub fn attach<H: Hw>(facts: attach::PciFacts, hw: H) -> (Card, H) {
        let mut owner = Bringup::new(hw, policy::GATE, 64);
        let t0 = owner.now_us();
        let state = attach::attach(&facts, &mut owner);
        let took_us = owner.now_us().saturating_sub(t0);
        let totals = owner.log().totals();
        (Card { facts, state, totals, took_us }, owner.into_hw())
    }

    /// The attach passed and nothing was written or refused.
    pub fn clean(&self) -> bool {
        self.state.is_ok() && self.totals == Totals::default()
    }
}

/// A device's place for its card: attached once, by whoever asks first. The attach may sleep up to 2 s (a card
/// whose own firmware is not ready, likely one the UEFI did not start), and `ksync::Once` makes its waiters spin, so
/// the attach runs under a sleeping lock and only its finished result goes into the `Once`: a caller that arrives
/// meanwhile sleeps.
pub struct CardSlot {
    attaching: Mutex<()>,
    card: Once<Card>,
}

impl CardSlot {
    pub const fn new() -> CardSlot {
        CardSlot { attaching: Mutex::named("amdgpu.rdna4.attach", ()), card: Once::new() }
    }

    /// The card if an attach has finished (never waits).
    pub fn get(&self) -> Option<&Card> {
        self.card.get()
    }

    /// The card, attached by `attach` if nobody did yet (thread context: it may sleep, for the lock or in `attach`).
    /// `attach` must not read PCI configuration space: those locks are outside this one in the lock order.
    pub fn get_or_attach(&self, attach: impl FnOnce() -> Card) -> &Card {
        if let Some(card) = self.card.get() {
            return card;
        }
        let _one = self.attaching.lock();
        match self.card.get() {
            Some(card) => card,
            None => {
                let card = attach();
                self.card.get_or_init(|| card)
            }
        }
    }

    /// Callers asleep for a running attach (tests).
    pub fn waiting(&self) -> usize {
        self.attaching.waiters()
    }

    /// An attach is running now.
    pub fn attaching(&self) -> bool {
        self.attaching.is_locked()
    }
}

impl Default for CardSlot {
    fn default() -> Self {
        CardSlot::new()
    }
}
