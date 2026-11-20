//! Wave 43 (piece 12): the stage `sdma`, SDMA 7.0.1's two engines started as Linux's `sdma_v7_0_hw_init` starts
//! them (sdma_v7_0.c:1409-1420 -> `sdma_v7_0_start` :820-856). Trace section `sdma` (init.txt lines 9604-9671).
//!
//! | Linux | what |
//! |---|---|
//! | `sdma_v7_0_enable(adev, true)` :444-462 | MCU_CNTL of both engines, read and written with HALT 0 |
//! | `sdma_v7_0_gfx_resume_instance` :474-622, engine 0 then engine 1 | RB_CNTL with the ring's size, the four pointers, the poll and read-pointer addresses, the base, MINOR_PTR_UPDATE around the write pointer, the doorbell and its offset (and NBIF's doorbell entry, engine 0 only), the watchdog, UTCL1, the unhalt, the ring's enable, the IBs' enable |
//! | `amdgpu_ring_test_helper` :617 -> `sdma_v7_0_ring_test_ring` :917-968 | right after each engine's IB enable: 0xCAFEDEAD into a write-back slot, a WRITE_LINEAR of 0xDEADBEEF on the ring, the doorbell, the slot polled at most 100000 times 1 us apart. No register access: the trace shows it as 112 and 25 us of silence |
//!
//! What Linux did in `sdma_v7_0_sw_init` (:1294-1357) and before comes first, as far as the card's host side sees
//! it: the rings (8 KiB each), the write-back slots and the IB pool are system pages behind the GART
//! (amdgpu_ring.c:360-366, amdgpu_wb.c:61-83, amdgpu_sa.c:54-55). Linux bound them while the blocks set up
//! (section `sw-init`); BitsOS binds them here, right before the first register that names them, with Linux's
//! flush after a bind (`linuxseq`'s `Rule::BindBefore`). Every address is BitsOS's own and a declared rule
//! (`sdmaseq`).
//!
//! The stage's place is the chain's (`stage::in_order`: after `gchub`; later waves put the command processor and
//! MES between that and this one). Besides it asks for what is its own, by state and never by the name of the
//! stage before it: the GART is on, the secure processor loaded this engine's firmware and the autoload went
//! out, the DMAR survey says yes, the power controller's features are on in this boot (`pmfw::State::gc_powered`;
//! the fake refuses the first write without power and a complete autoload on its own account,
//! `Model::gc_awake`). Both engines' addresses go through GFXHUB (`vm_hub`, sdma_v7_0.c:1328), which must be on
//! (the session's `gc.hub`), so the bind's flush covers both hubs; since wave 44 MES runs when the stage does
//! (Linux's order), and the flush is Linux's form after MES (`mesflush`); on the fake a ring on a hub that is off
//! names pages no walker finds.
//! A second run is refused, and so is a run after one that stopped once it had written a register: the engines
//! are then in no state Linux's cold start knows (a Reload halts them from the stage's first write on, `sdmaoff`).

use super::guard::{Allow, Guard, HostAsk};
use super::nb::Io;
use super::nbregs::NbRegs;
use super::sdmaq::{self, Place, QStop, Queue, lay};
use super::sdmaregs::SdmaRegs;
use super::stage::{Ctx, Session, Stage, Stop, psp_stop, read_file, stop};
use super::{fwlist, policy};
use crate::gpucore::fwload::FwSource;
use crate::gpucore::psp::{self, Hw};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::sdma as pkt;
use amdgpu_fmt::wb::WbPool;
use core::ops::Range;
use rdna4_core::regmap::Reg;
use rdna4_core::sdma7::{self, INSTANCES};
use rdna4_core::{hub41, ih7, pte12};

/// The engines after a passed `sdma` stage.
#[derive(Debug)]
pub struct Up {
    pub place: Place,
    pub q: [Queue; 2],
    pub pool: WbPool,
    /// The firmware's feature version, which decides the NOP's form.
    pub feature: u32,
    /// Reads of the slot each engine's ring test took.
    pub ring_test: [u32; 2],
}

/// The block in this boot.
#[derive(Debug, Default)]
pub struct State {
    /// The block's piece of system memory, kept for the boot once taken (`sdmaq::lay`).
    pub piece: Option<u64>,
    /// A register of the block was written in this boot.
    pub tried: bool,
    /// The Reload hook's word (`livesdma`, `sdmaoff::Written`), set together with `tried`; none on a card with
    /// no hook (the model).
    pub reload: Option<super::sdmaoff::Written>,
    pub up: Option<Up>,
    /// What the stage `sdmatest` took for its rungs and has not given back: (VRAM range, the GART address of the
    /// test pages).
    pub held: (Option<Range<u64>>, Option<u64>),
    /// The last `sdmatest`'s rungs: (name, passed).
    pub rungs: Vec<(&'static str, bool)>,
    /// Where the stage bound the engines' rings and slots (`sdmaseq`'s rules hold their addresses to these).
    pub bound: Option<super::sdmaseq::Bound>,
}

impl State {
    pub const fn new() -> State {
        State { piece: None, tried: false, reload: None, up: None, held: (None, None), rungs: Vec::new(), bound: None }
    }

    pub fn describe(&self) -> String {
        match (&self.up, self.tried) {
            (Some(u), _) => format!(
                "sdma on (rings at mc {:#x} and {:#x}, ring tests after {} and {} reads, {} doorbells, rungs {:?})",
                u.place.ring_mc(0),
                u.place.ring_mc(1),
                u.ring_test[0],
                u.ring_test[1],
                u.q[0].rung.len() + u.q[1].rung.len(),
                self.rungs
            ),
            (None, true) => String::from("sdma stopped half-way (its registers were written; not again in this boot)"),
            (None, false) => String::from("sdma not run"),
        }
    }
}

pub fn is_mine(stage: Stage) -> bool {
    matches!(stage, Stage::Sdma | Stage::SdmaTest)
}

/// The stage's own order check: once in a boot, and not after a run that stopped with registers written.
pub fn order(s: &State) -> Result<(), Stop> {
    if s.up.is_some() {
        return Err(stop("die Kopiereinheit der Karte läuft schon", "sdma ran in this boot: a second run is refused"));
    }
    if s.tried {
        return Err(stop("die Kopiereinheit wurde in diesem Start schon angefasst und blieb stehen: erst neu starten", "sdma stopped half-way in this boot after it wrote registers: the engines are in no state of Linux's cold start"));
    }
    Ok(())
}

/// The registers `gfx_resume_instance` writes for one engine.
const RESUME: [Reg; 18] = [
    sdma7::MCU_CNTL, sdma7::RB_CNTL, sdma7::RB_RPTR, sdma7::RB_RPTR_HI, sdma7::RB_WPTR, sdma7::RB_WPTR_HI, sdma7::RB_WPTR_POLL_ADDR_LO, sdma7::RB_WPTR_POLL_ADDR_HI, sdma7::RB_RPTR_ADDR_HI,
    sdma7::RB_RPTR_ADDR_LO, sdma7::RB_BASE, sdma7::RB_BASE_HI, sdma7::MINOR_PTR_UPDATE, sdma7::DOORBELL, sdma7::DOORBELL_OFFSET, sdma7::WATCHDOG_CNTL, sdma7::UTCL1_CNTL, sdma7::UTCL1_PAGE,
];

/// What the block's two stages may write: `sdma` Linux's writes of its section, the bind's flush (both hubs',
/// `Session::flush_regs`) and the GART table for the bind; `sdmatest` no register of the engines at all, only
/// the flush of the GART map for its test pages, the IH rings' control (the reader's overflow clear), the two
/// hubs' fault control (the reader's clear of a page fault, `ihfault`) and its VRAM test range. Taking that range is
/// done here, before the guard exists, so the guard can list it (as `gmc` plans before its owner).
pub fn allow(stage: Stage, s: &mut Session, n: &NbRegs, sr: Option<&SdmaRegs>) -> Allow {
    let flush = s.flush_written(n);
    let table = s.nb.gart.as_ref().map(|g| g.table_mc());
    let mut regs = flush;
    let mut mem: Vec<Range<u64>> = table.into_iter().collect();
    match (stage, sr) {
        (Stage::Sdma, Some(sr)) => {
            for i in 0..INSTANCES {
                regs.extend(sr.bytes(&RESUME, i));
                regs.extend(sr.bytes(&[sdma7::IB_CNTL], i));
            }
            regs.extend(sr.nbif(sdma7::DOORBELL_ENTRY_2));
        }
        (Stage::SdmaTest, Some(_)) => {
            regs.extend([ih7::RB_CNTL, ih7::RB_CNTL_RING1, hub41::GC.fault_cntl, hub41::MM.fault_cntl].into_iter().filter_map(|r| n.byte(r)));
            if let Some(range) = super::sdmarungs::take_vram(s) {
                mem.push(range);
            }
        }
        _ => {}
    }
    Allow { stage: stage.name(), regs, mem }
}

/// The registers Linux's interrupt handlers reach, which the IH reader `sdmatest` runs itself may touch while it
/// waits for a trap: a ring's overflow clear (`ih_v7_0_get_wptr`, ih_v7_0.c:438-482: the write pointer's register
/// and the ring's control) and a page fault's decode (`gmc_v12_0_process_interrupt`, gmc_v12_0.c:120-131: the
/// faulting hub's status and its fault control). They are Linux's handlers' accesses, not the rungs' sequence,
/// and the call's comparison with the trace leaves them out and counts them (`sdmaseq::rungs_verdict`).
pub fn irq_regs(n: &NbRegs) -> Vec<u32> {
    [ih7::RB_WPTR, ih7::RB_CNTL, ih7::RB_WPTR_RING1, ih7::RB_CNTL_RING1, hub41::GC.fault_status, hub41::GC.fault_cntl, hub41::MM.fault_status, hub41::MM.fault_cntl].into_iter().filter_map(|r| n.byte(r)).collect()
}

/// The guard of one of the block's stages: its doorbells and the system memory it may write. Any other stage's
/// guard comes back as it was.
pub fn guarded<H: Hw>(g: Guard<H>, stage: Stage, s: &Session) -> Guard<H> {
    let piece = |bytes: u64| s.sdma.piece.map(|at| at..at + bytes);
    let engines = [sdma7::doorbell_dw(0), sdma7::doorbell_dw(1)];
    // Wave 44: a bind's flush through MES rings the KIQ pipe's doorbell and writes MES's ring and slots.
    let (mes_bells, mes_sys) = super::messtage::flush_extras(s);
    match stage {
        // What the stage takes itself is added by the guard as it hands it out.
        Stage::Sdma => g.with_host(alloc::vec![HostAsk::Pages]).with_doorbells(engines.into_iter().chain(mes_bells).collect()).with_sys(piece(lay::PIECE).into_iter().chain(mes_sys).collect()),
        Stage::SdmaTest => {
            // The reader the stage runs itself writes each ring's read pointer slot and rings the IH's doorbells.
            let ih = s.nb.ih.as_ref().map(|i| i.place.sys + 2 * u64::from(ih7::RING_BYTES)).map(|wb| wb..wb + super::ihring::IhPlace::WB_BYTES);
            g.with_doorbells([engines[0], engines[1], ih7::DOORBELL_RING0, ih7::DOORBELL_RING1].into_iter().chain(mes_bells).collect()).with_sys(piece(lay::PIECE).into_iter().chain(ih).chain(mes_sys).collect())
        }
        _ => g,
    }
}

/// Asked by `fw` before it loads anything a second time in one boot. The SMU's firmware and the autoload start
/// the GC block anew, and with it the engines' firmware. Linux does that only at a resume, after its suspend
/// stopped the engines (`sdma_v7_0_hw_fini`, sdma_v7_0.c:1422-1434, before `gfx_v12_0_hw_fini`: the blocks'
/// reverse order, amdgpu_device.c:3172-3217), and starts them again afterwards. That stop and second start
/// inside one boot is not built: this wave is Linux's cold start, and what the engines' registers hold after a
/// stop and a second autoload is in no trace. So once a register of the engines was written in this boot, `fw`
/// is refused before any access; a Reload stops the engines (`sdmaoff`) and the next kernel begins at `nbif`.
pub fn before_fw(s: &State) -> Result<(), Stop> {
    if s.tried {
        return Err(stop(
            "die Kopiereinheit der Karte wurde in diesem Start schon gestartet: erst neu starten",
            "sdma wrote the copy engines' registers in this boot: a second fw would load the SMU's firmware and send the autoload under engines that may run (Linux stops them first, sdma_v7_0_hw_fini, and that stop and second start in one boot is not built); Reload first",
        ));
    }
    Ok(())
}

pub(super) fn q_stop(e: QStop) -> Stop {
    let plain = match &e {
        QStop::Mem { .. } => "der Arbeitsspeicher für die Kopiereinheit lässt sich nicht beschreiben",
        QStop::Doorbell { .. } => "die Kopiereinheit lässt sich nicht anstoßen",
        QStop::Ring(_) | QStop::Slots(_) => "unerwarteter Fehler im Befehlskanal der Kopiereinheit",
    };
    stop(plain, format!("{e:?}"))
}

fn rd(io: &mut Io, byte: u32) -> Result<u32, Stop> {
    psp::read(io.card.hw(), byte).map_err(psp_stop)
}

fn wr(io: &mut Io, byte: u32, v: u32) -> Result<(), Stop> {
    psp::write(io.card.hw(), byte, v).map_err(psp_stop)
}

/// The secure processor loaded this engine's firmware and the autoload went out (`psp_load_non_psp_fw` with the
/// autoload right after RLC_G, amdgpu_psp.c:3531-3590): the MCU the stage unhalts has a program.
fn firmware_loaded(s: &Session) -> bool {
    s.autoload && s.loaded.iter().any(|l| l.0 == fwlist::Id::SdmaRs64)
}

/// `ucode_feature_version` of the stick's file (`amdgpu_sdma_init_microcode`, amdgpu_sdma.c:175-183).
fn feature_version(c: &Ctx, files: &mut dyn FwSource) -> Result<u32, Stop> {
    let (plain, _) = fwlist::FILES.iter().find(|(_, role)| *role == fwlist::Role::Sdma).ok_or_else(|| stop("eine Datei fehlt auf dem Stick", "no SDMA file in the list"))?;
    let b = read_file(files, fwlist::name_for(plain, c.facts.device, c.facts.revision))?;
    let at = sdma7::FEATURE_VERSION_AT;
    b.get(at..at + 4).and_then(|w| w.try_into().ok()).map(u32::from_le_bytes).ok_or_else(|| stop("eine Datei auf dem Stick passt nicht", "the SDMA file is shorter than its header"))
}

/// `sdma_v7_0_ring_test_ring` for one engine; the reads of the slot it took.
fn ring_test(q: &mut Queue, pool: &mut WbPool, io: &mut Io, p: &Place) -> Result<u32, Stop> {
    let slot = pool.get().map_err(|e| q_stop(QStop::Slots(e)))?;
    let at = p.slot_sys(slot);
    sdmaq::sys_write(io.card, at, &sdma7::POISON.to_le_bytes()).map_err(q_stop)?;
    let mut words = Vec::new();
    pkt::write_linear(&mut words, p.slot_mc(slot), &[sdma7::RING_TEST]).map_err(|e| q_stop(QStop::Ring(e.into())))?;
    q.submit(io.card, p, &words).map_err(q_stop)?;
    let mut last = sdma7::POISON;
    let mut reads = 0;
    while reads < sdma7::USEC_TIMEOUT {
        last = sdmaq::sys_read32(io.card, at).map_err(q_stop)?;
        reads += 1;
        if last == sdma7::RING_TEST {
            break;
        }
        io.card.hw().delay_us(1);
    }
    // Linux frees the slot whatever came of the test.
    pool.free(slot).map_err(|e| q_stop(QStop::Slots(e)))?;
    if last != sdma7::RING_TEST {
        klog::event!(Error, "gx", "rdna4.sdma.ring_test", engine = q.me, reads = reads, last = klog::Hex(u64::from(last)); "the ring test's slot never took the engine's write");
        return Err(stop("die Kopiereinheit antwortet nicht", format!("engine {}: the ring test's slot reads {last:#010x} after {reads} reads 1 us apart, not {:#010x} (Linux gives up there too); the stage `state` shows the engine's registers", q.me, sdma7::RING_TEST)));
    }
    Ok(reads)
}

/// `sdma_v7_0_gfx_resume_instance` for engine `i`, with its ring test.
fn resume(i: u32, sr: &SdmaRegs, io: &mut Io, p: &mut Option<Place>, bind: &mut dyn FnMut(&mut Io) -> Result<Place, Stop>, q: &mut Queue, pool: &mut WbPool) -> Result<u32, Stop> {
    let b = |r: Reg| sr.byte(r, i).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", format!("{} has no place in BAR5", r.name)));
    let v = rd(io, b(sdma7::RB_CNTL)?)?;
    let sized = sdma7::rb_cntl_sized(v);
    wr(io, b(sdma7::RB_CNTL)?, sized)?;
    for r in [sdma7::RB_RPTR, sdma7::RB_RPTR_HI, sdma7::RB_WPTR, sdma7::RB_WPTR_HI] {
        wr(io, b(r)?, 0)?;
    }
    // The first register that names the block's memory: bound right before it, once for both engines.
    let place = match *p {
        Some(place) => place,
        None => *p.insert(bind(io)?),
    };
    let poll = place.slot_mc(q.slots.wptr);
    wr(io, b(sdma7::RB_WPTR_POLL_ADDR_LO)?, poll as u32)?;
    wr(io, b(sdma7::RB_WPTR_POLL_ADDR_HI)?, (poll >> 32) as u32)?;
    let rptr = place.slot_mc(q.slots.rptr);
    wr(io, b(sdma7::RB_RPTR_ADDR_HI)?, (rptr >> 32) as u32)?;
    wr(io, b(sdma7::RB_RPTR_ADDR_LO)?, rptr as u32 & 0xFFFF_FFFC)?;
    let base = place.ring_mc(i);
    wr(io, b(sdma7::RB_BASE)?, (base >> 8) as u32)?;
    wr(io, b(sdma7::RB_BASE_HI)?, (base >> 40) as u32)?;
    // "before programing wptr to a less value, need set minor_ptr_update first".
    wr(io, b(sdma7::MINOR_PTR_UPDATE)?, 1)?;
    wr(io, b(sdma7::RB_WPTR)?, 0)?;
    wr(io, b(sdma7::RB_WPTR_HI)?, 0)?;
    let doorbell = rd(io, b(sdma7::DOORBELL)?)?;
    let offset = rd(io, b(sdma7::DOORBELL_OFFSET)?)?;
    wr(io, b(sdma7::DOORBELL)?, sdma7::doorbell(doorbell))?;
    wr(io, b(sdma7::DOORBELL_OFFSET)?, sdma7::doorbell_offset(offset, sdma7::doorbell_dw(i)))?;
    if i == 0 {
        // nbif_v6_3_1_sdma_doorbell_range: one entry for both engines.
        let entry = sr.nbif(sdma7::DOORBELL_ENTRY_2).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", "NBIF's doorbell entry has no place in BAR5"))?;
        let v = rd(io, entry)?;
        wr(io, entry, sdma7::doorbell_range(v))?;
    }
    wr(io, b(sdma7::MINOR_PTR_UPDATE)?, 0)?;
    let v = rd(io, b(sdma7::WATCHDOG_CNTL)?)?;
    wr(io, b(sdma7::WATCHDOG_CNTL)?, sdma7::watchdog(v))?;
    let v = rd(io, b(sdma7::UTCL1_CNTL)?)?;
    wr(io, b(sdma7::UTCL1_CNTL)?, sdma7::utcl1_cntl(v))?;
    let v = rd(io, b(sdma7::UTCL1_PAGE)?)?;
    wr(io, b(sdma7::UTCL1_PAGE)?, sdma7::utcl1_page(v))?;
    let v = rd(io, b(sdma7::MCU_CNTL)?)?;
    wr(io, b(sdma7::MCU_CNTL)?, sdma7::mcu_unhalt(v))?;
    wr(io, b(sdma7::RB_CNTL)?, sdma7::rb_cntl_run(sized, true))?;
    let v = rd(io, b(sdma7::IB_CNTL)?)?;
    wr(io, b(sdma7::IB_CNTL)?, sdma7::ib_cntl(v, true))?;
    ring_test(q, pool, io, &place)
}

pub fn run(s: &mut Session, stage: Stage, c: &Ctx, io: &mut Io, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    match stage {
        Stage::Sdma => start(s, c, io, files, out),
        _ => super::sdmarungs::run(s, c, io, out),
    }
}

fn start(s: &mut Session, c: &Ctx, io: &mut Io, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    order(&s.sdma)?;
    if s.nb.gart.is_none() {
        return Err(stop("zuerst den Speicher der Karte einrichten", "no GART: the engines' rings lie behind it"));
    }
    if !firmware_loaded(s) {
        return Err(stop("zuerst die Steuerprogramme laden", "the secure processor has not loaded SDMA's firmware (LOAD_IP_FW type 71) and sent the autoload in this boot"));
    }
    // Linux's order puts the power controller's start (`smu`) before every GC block; here that is a fact of
    // this boot besides its place in the chain, so a chain put together wrongly still writes nothing into a
    // block without power.
    if !s.pmfw.gc_powered() {
        return Err(stop("zuerst die Energieverwaltung der Karte starten", "the power controller's features are not on in this boot (EnableAllSmuFeatures was not answered): no register of the copy engines is read or written before it"));
    }
    // The hub the engines sit on, by this boot's record (since wave 44 MES stands between `gchub` and this stage
    // in the chain, so the order alone no longer names it).
    if !s.gc.hub {
        return Err(stop("zuerst den Speicher des Grafikteils einrichten", "GFXHUB is not on in this boot: the engines' rings and slots are reached through it"));
    }
    let sr = SdmaRegs::from(&c.att.discovery).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", "GC or NBIF bases missing, or an SDMA register past BAR5"))?;
    let feature = feature_version(c, files)?;
    let burst = feature >= sdma7::BURST_NOP_FEATURE;
    // The host side first: a no here leaves the card's registers untouched.
    let survey = io.card.dma_survey();
    policy::may_send(io.driven, survey).map_err(|why| stop("der Rechner lässt die Karte nicht an den Arbeitsspeicher", why))?;
    let sys = match s.sdma.piece {
        Some(sys) => sys,
        None => io.card.pages(lay::PIECE).ok_or_else(|| stop("kein Arbeitsspeicher für die Karte frei", "no pages for the SDMA rings"))?,
    };
    s.sdma.piece = Some(sys);
    // What Linux's sw_init left: the queues with their slots, the rings cleared, the write-back page at zero.
    let mut pool = WbPool::new(sdmaq::WB_SLOTS);
    let mut q = [Queue::new(0, burst, &mut pool).map_err(q_stop)?, Queue::new(1, burst, &mut pool).map_err(q_stop)?];
    let cpu = Place { sys, mc: 0 };
    sdmaq::sys_write(io.card, sys + lay::WB, &alloc::vec![0u8; pte12::PAGE as usize]).map_err(q_stop)?;
    for x in &q {
        x.init_memory(io.card, &cpu).map_err(q_stop)?;
    }
    out.push(format!("sdma: system memory at {sys:#x} ({} bytes: two rings of {} bytes, the write-back page, the IB page, {} test pages); firmware feature version {feature}, so the NOP is {}", lay::PIECE, sdma7::RING_BYTES, lay::TEST_BYTES / pte12::PAGE, if burst { "counted" } else { "plain" }));
    s.sdma.tried = true;
    // From here a Reload stops the engines as Linux's hw_fini does, whatever comes of the stage.
    if let Some(hook) = &s.sdma.reload {
        hook.set();
    }
    // sdma_v7_0_start: "unhalt the MEs".
    for i in 0..INSTANCES {
        let b = sr.byte(sdma7::MCU_CNTL, i).ok_or_else(|| stop("Bauteilliste nennt die Register nicht", "SDMA MCU_CNTL has no place in BAR5"))?;
        let v = rd(io, b)?;
        wr(io, b, sdma7::mcu_halt(v, false))?;
    }
    // The flush in this boot's form: through MES once it runs (Linux's order puts MES before the engines).
    let mut tlb = super::mesflush::tlb(io.flush()?, &mut s.mes);
    let gart = &mut s.nb.gart;
    let mut bound = Vec::new();
    let slots = [q[0].slots, q[1].slots];
    let record = &mut s.sdma.bound;
    let mut bind = |io: &mut Io| -> Result<Place, Stop> {
        let g = gart.as_mut().ok_or_else(|| stop("Reihenfolge stimmt nicht", "no GART"))?;
        let mc = g.map_with(io.card, &mut tlb, "SDMA rings", &super::nb::pages_of(sys, lay::BOUND), &mut bound).map_err(|e| stop("der Speicher für die Kopiereinheit lässt sich nicht einbinden", format!("{e:?}")))?;
        let place = Place { sys, mc };
        *record = Some(super::sdmaseq::Bound::of(&place, &slots));
        Ok(place)
    };
    let mut place = None;
    let mut ring_test = [0; 2];
    for i in 0..INSTANCES {
        let n = resume(i, &sr, io, &mut place, &mut bind, &mut q[i as usize], &mut pool)?;
        ring_test[i as usize] = n;
        let p = place.ok_or_else(|| stop("Reihenfolge stimmt nicht", "no place"))?;
        let rptr = q[i as usize].read_rptr(io.card, &p).map_err(q_stop)?;
        out.push(format!(
            "sdma{i}: ring at mc {:#x}, read pointer slot at mc {:#x}, poll slot at mc {:#x}, doorbell dword {:#x}; ring test passed after {n} reads of its slot (Linux gives up after {}), read pointer slot {rptr:#x}",
            p.ring_mc(i),
            p.slot_mc(q[i as usize].slots.rptr),
            p.slot_mc(q[i as usize].slots.wptr),
            sdma7::doorbell_dw(i),
            sdma7::USEC_TIMEOUT
        ));
    }
    let place = place.ok_or_else(|| stop("Reihenfolge stimmt nicht", "no place"))?;
    out.extend(bound);
    // Linux registers the trap's source in sw_init (:1302-1306): from here its entries are the engines'.
    if let Some(ih) = s.nb.ih.as_mut() {
        super::sdmaih::register(ih);
    }
    s.sdma.up = Some(Up { place, q, pool, feature, ring_test });
    Ok(())
}
