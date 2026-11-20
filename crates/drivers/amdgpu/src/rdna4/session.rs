//! One card's bring-up in this boot: what its stages did so far and what the next one stands on. The chain's
//! order is `Stage::CHAIN`; `passed`, `before` and `next` are the one place that says which stage passed, and
//! `stage::in_order` the one place that refuses a stage of the chain for it.

use super::fwlist::Id;
use super::nb;
use super::ring14::{Pri, Reserved, Ring14};
use super::smu14::SmuFacts;
use super::stage::Stage;
use super::vram::{Bump, Layout};
use crate::gpucore::handoff::StopNote;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use rdna4_core::bootnote::BootNote;
use rdna4_core::bootseq;

/// How the secure OS came to run in this boot, or how far its load came.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sos {
    /// The stage `sos` loaded it through the bootloader: a fresh secure OS, which has no ring.
    Loaded { sol: u32 },
    /// It ran already when `sos` looked (C2PMSG_81 not 0): nothing was written, and a ring from before may exist.
    Warm { sol: u32 },
    /// A part was not taken. The bootloader keeps what it got: nothing more is sent in this boot.
    Failed { part: bootseq::Part, c35: u32 },
}

/// A trusted application as the secure OS answered its load (Linux's `ta_context`: `session_id`, `resp_status`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaLoaded {
    pub name: &'static str,
    pub session: u32,
    pub status: u32,
}

/// One card's bring-up in this boot (`Device::stages`).
#[derive(Default)]
pub struct Session {
    /// Where VRAM lies and the secure processor's window in it: planned by `gmc` from the card's own registers.
    pub layout: Option<Layout>,
    /// The card's VRAM manager of this boot (made by `gmc`, the first stage that needs VRAM): the picture, the
    /// secure processor's window and the firmware's own region are kept from what it hands out.
    pub bump: Option<Bump>,
    /// Wave 42: NBIF (with the HDP flush's remap, where every later stage flushes), the GART and the IH rings.
    pub nb: nb::State,
    /// Wave 43: the graphics block's start and its hub (`gcstage`).
    pub gc: super::gcstage::State,
    /// The bootloader's state as this boot saw it (`rdna4_core::bootseq`), lent to `sos`'s guard.
    pub(super) boot: Option<bootseq::Gate>,
    /// What the kernel before this Reload did to the bootloader in this power cycle (`bootnote`; the glue sets it).
    pub boot_before: Option<BootNote>,
    pub sos: Option<Sos>,
    pub(super) pri: Option<Pri>,
    pub(super) ring: Option<Ring14>,
    pub reserved: Option<[Reserved; 2]>,
    pub tmr_size: Option<u32>,
    pub tmr_done: bool,
    /// (image, MC address, bytes, where the PSP put it).
    pub loaded: Vec<(Id, u64, u32, u64)>,
    /// The images of the list that follow the autoload, already in VRAM: (image, MC address, bytes).
    pub(super) pending: Vec<(Id, u64, u32)>,
    pub fw_done: bool,
    pub autoload: bool,
    pub rest_done: bool,
    pub tas: Vec<TaLoaded>,
    pub ta_done: bool,
    /// The first SMU message of this boot went out (Linux skips the pre-poll only for it).
    pub smu_asked: bool,
    pub smu: Option<SmuFacts>,
    /// Wave 43: the power controller's start and what it learned (`pmfw`).
    pub pmfw: super::pmfw::State,
    /// Wave 44: the graphics block's command processor (`cpstage`).
    pub cp: super::cpstage::State,
    /// Wave 44: MES and the kernel queues mapped through it (`messtage`, `meskq`).
    pub mes: super::messtage::State,
    /// Wave 44: the kernel rings and their tests (`ringstage`, `ringrungs`).
    pub rings: super::ringstage::State,
    /// Wave 43: SDMA's two engines (`sdmastage`).
    pub sdma: super::sdmastage::State,
    /// The note of this boot's last ring change (also handed to the next kernel under the card's own tag).
    pub note: Option<StopNote>,
    /// What the card saw of the trace section whose stages run now: each stage call is held against Linux's trace
    /// with it (`chainseq::verdict`). A ring's destroy ends the section with the ring.
    pub(super) seq: super::chainseq::Seen,
}

impl Session {
    pub fn ring_up(&self) -> bool {
        self.ring.as_ref().is_some_and(Ring14::created)
    }

    /// How many of Linux's nine bootloader commands went out in this boot.
    pub fn boot_sent(&self) -> usize {
        self.boot.as_ref().map_or(0, bootseq::Gate::sent)
    }

    /// The 1 MiB firmware buffer's MC address, once a stage made it.
    pub fn pri_mc(&self) -> Option<u64> {
        self.pri.as_ref().map(|p| p.buf.mc)
    }

    /// What of VRAM the stages handed out (VRAM offsets).
    pub fn given(&self) -> &[core::ops::Range<u64>] {
        self.bump.as_ref().map_or(&[], |b| b.given.as_slice())
    }

    /// What this boot's stages did to the card, in one line (the stage `state` and the card's snapshot show it).
    pub fn summary(&self) -> String {
        format!(
            "next of the chain {}; {}, window {}, bootloader commands {} of 9{}, secure os {:?}, ring {}, reserved {:?}, toc {:?}, tmr {}, images {} (+{} after the autoload), autoload {}, trusted applications {:?}, smu {}, power controller {}, {}, {}, {}, {}, {}; {}",
            self.next().map_or("none (it stands)", Stage::name),
            self.nb.describe(),
            self.layout.as_ref().map_or(String::from("none"), |l| format!("{:#x}..{:#x} (mc {:#x})", l.window.start, l.window.end, l.mc_base)),
            self.boot_sent(),
            self.boot_before.map_or(String::new(), |n| format!(" (the kernel before this Reload {n})")),
            self.sos,
            if self.ring_up() { "up" } else { "none" },
            self.reserved,
            self.tmr_size,
            self.tmr_done,
            self.loaded.len(),
            self.pending.len(),
            self.autoload,
            self.tas,
            if self.smu.is_some() { "asked" } else { "not asked" },
            self.pmfw.describe(),
            self.gc.describe(),
            self.cp.describe(),
            self.mes.describe(),
            self.rings.describe(),
            self.sdma.describe(),
            self.nb.ih.as_ref().map_or(String::from("thermal taker: no IH in this boot"), |i| i.reader.thermal.describe())
        )
    }

    /// Whether a stage of Linux's chain passed in this boot.
    pub fn passed(&self, stage: Stage) -> bool {
        match stage {
            Stage::Nbif => self.nb.nbif,
            Stage::Gmc => self.nb.gart.is_some(),
            Stage::Ih => self.nb.ih.is_some(),
            Stage::Sos => matches!(self.sos, Some(Sos::Loaded { .. } | Sos::Warm { .. })),
            Stage::Ring => self.ring_up() && self.reserved.is_some(),
            Stage::Toc => self.tmr_size.is_some(),
            Stage::Tmr => self.tmr_done,
            Stage::Fw => self.fw_done,
            Stage::Autoload => self.autoload,
            Stage::FwRest => self.rest_done,
            Stage::Ta => self.ta_done,
            // Not while `fw` is half-way through a load of the PMFW's firmware: the graphics block was started
            // anew for it, and `gcboot` stands on the start of the PMFW that load brings.
            Stage::Pmfw => self.pmfw.passed && !self.pmfw.reload_began,
            Stage::GcBoot => self.gc.boot.is_some(),
            Stage::GcHub => self.gc.hub,
            Stage::GcCp => self.cp.passed,
            Stage::Mes => self.mes.ready(),
            Stage::MesQ => self.mes.kq,
            Stage::GcRing => self.rings.passed,
            Stage::Sdma => self.sdma.up.is_some(),
            _ => false,
        }
    }

    /// The stage of Linux's chain that must have passed in this boot before `stage` runs, and whether it did
    /// (`None`: the chain's first, or a look outside the chain).
    pub fn before(&self, stage: Stage) -> Option<(Stage, bool)> {
        let at = Stage::CHAIN.iter().position(|x| *x == stage)?;
        let prev = *Stage::CHAIN.get(at.checked_sub(1)?)?;
        Some((prev, self.passed(prev)))
    }

    /// The chain's next stage in this boot: the first that did not pass yet.
    pub fn next(&self) -> Option<Stage> {
        Stage::CHAIN.into_iter().find(|x| !self.passed(*x))
    }

    /// The registers of a TLB flush as this boot stands: MMHUB's, and GFXHUB's with them once `gchub` passed, so
    /// every bind and unbind from then on flushes both hubs as Linux does (amdgpu_gart.c:504-505; `gart12`).
    /// GFXHUB leaves the flush only after it was switched off (`gcstage::before_fw`), as in Linux
    /// (`gfx.is_poweron`, gfx_v12_0.c:3917-3919). Written by the CPU until MES runs; from the stage `mes` on the
    /// hubs' requests go through MES as Linux's do once `mes.ring[0].sched.ready` (gmc_v12_0.c:315-327): the form
    /// is `mesflush::tlb` over these registers, and what the CPU itself writes then is `flush_written`.
    pub fn flush_regs(&self, n: &super::nbregs::NbRegs) -> Option<super::gart12::FlushRegs> {
        let f = super::gart12::FlushRegs::of(n, self.nb.hdp_flush(n.hdp_flush))?;
        if self.gc.hub { f.with_gc(n) } else { Some(f) }
    }

    /// What a TLB flush of this boot writes through the stage's guard: the register form's registers before MES
    /// runs, the HDP flush register alone after (the hubs' requests are then the firmware's writes).
    pub fn flush_written(&self, n: &super::nbregs::NbRegs) -> Vec<u32> {
        self.flush_regs(n).map(|f| super::mesflush::written(&f, self.mes.ready())).unwrap_or_default()
    }

    /// What stays when a ring is gone: the card's layout and its VRAM manager (the window comes back whole, the
    /// GART table and the firmware's region stay where they are), NBIF, the GART and the IH, the graphics block
    /// (its engines' starts and its hub are the card's state, which a ring's destroy does not touch; a second
    /// `fw` on the next ring is what starts the block anew, `gcstage::before_fw`) and its command processor, what the
    /// bootloader got and how the secure OS runs, the SMU's answers and the power controller's start (the PMFW
    /// runs on until `fw` loads its firmware again), the note; MES and SDMA's engines, which no ring command stops.
    pub(super) fn after_destroy(&mut self, note: StopNote) {
        let sos = self.sos.map(|x| match x {
            // The secure OS now has had a ring: the next ring stage looks before it writes.
            Sos::Loaded { sol } => Sos::Warm { sol },
            other => other,
        });
        let bump = self.bump.take().map(|mut b| {
            b.window_back();
            b
        });
        *self = Session {
            layout: self.layout.take(),
            bump,
            nb: core::mem::take(&mut self.nb),
            gc: self.gc,
            cp: core::mem::take(&mut self.cp),
            mes: core::mem::take(&mut self.mes),
            rings: core::mem::take(&mut self.rings),
            sdma: core::mem::take(&mut self.sdma),
            boot: self.boot.take(),
            boot_before: self.boot_before,
            sos,
            smu_asked: self.smu_asked,
            smu: self.smu.take(),
            pmfw: core::mem::take(&mut self.pmfw),
            note: Some(note),
            ..Session::default()
        };
    }
}

pub struct SessionSlot(pub ksync::Mutex<Session>);

impl SessionSlot {
    pub const fn new() -> SessionSlot {
        SessionSlot(ksync::Mutex::named("amdgpu.rdna4.stage", Session {
            layout: None,
            bump: None,
            nb: nb::State::new(),
            gc: super::gcstage::State::new(),
            boot: None,
            boot_before: None,
            sos: None,
            pri: None,
            ring: None,
            reserved: None,
            tmr_size: None,
            tmr_done: false,
            loaded: Vec::new(),
            pending: Vec::new(),
            fw_done: false,
            autoload: false,
            rest_done: false,
            tas: Vec::new(),
            ta_done: false,
            smu_asked: false,
            smu: None,
            pmfw: super::pmfw::State::new(),
            cp: super::cpstage::State::new(),
            mes: super::messtage::State::new(),
            rings: super::ringstage::State::new(),
            sdma: super::sdmastage::State::new(),
            note: None,
            seq: super::chainseq::Seen::new(),
        }))
    }
}

impl Default for SessionSlot {
    fn default() -> Self {
        SessionSlot::new()
    }
}
