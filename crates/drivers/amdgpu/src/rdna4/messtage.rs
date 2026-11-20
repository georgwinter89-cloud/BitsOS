//! Wave 44, item ME (piece 14): the stage `mes`, MES 12 started as Linux's `mes_v12_0_kiq_hw_init` starts it
//! (mes_v12_0.c:1956-2016, entered from `gfx_v12_0_cp_resume` :3550) with the `mes_v12_0_hw_init` it calls
//! (:2036-2105). Trace section `mes` (init.txt lines 9502-9553 of 9502-9567; the rest is the stage `mesq`).
//!
//! | Linux | what | trace |
//! |---|---|---|
//! | `mes_v12_0_enable(adev, true)` :1303-1351 | per pipe under its selection: its reset bit, the program counter from the file, the pipe active; then 500 us | 9502-9514 |
//! | `mes_v12_0_queue_init(KIQ)` :1682-1731 | the KIQ pipe's MQD in memory, its HQD registers from it, the firmware version (CP_MES_GP3_LO) | 9515-9537 |
//! | `set_hw_resources(KIQ)`, `set_hw_resources_1(KIQ)` :2003-2011 | two commands on the KIQ ring, the second's answer not checked | none |
//! | `mes_v12_0_enable_unmapped_doorbell_handling` :1076-1094 | CP_UNMAPPED_DOORBELL | 9538-9539 |
//! | `mes_v12_0_queue_init(SCHED)` | the scheduler pipe's MQD, ADD_QUEUE of it on the KIQ ring, its version | 9540-9542 |
//! | `set_hw_resources(SCHED)`, `_1` | the masks, doorbells, bases; `_1` from version 0x4b, its answer not checked | none |
//! | `mes_v12_0_init_aggregated_doorbell` :1021-1074 | CP_MES_DOORBELL_CONTROL1..5, CP_HQD_GFX_CONTROL | 9543-9553 |
//! | `query_sched_status`, `amdgpu_mes_update_enforce_isolation` | QUERY_SCHEDULER_STATUS on the scheduler ring; MISC CHANGE_CONFIG on the KIQ ring | none |
//!
//! Then `mes.ring[0].sched.ready` (:2095-2096): from here every TLB flush after a GART change goes through MES
//! (`mesflush`). The KIQ setting before all of it (`mes_v12_0_kiq_setting`, init.txt 9500-9501) is the command
//! processor's stage's last step (the trace's section gc-cp ends with it).
//!
//! What Linux had from `sw_init` (mes_v12_0.c:1819-1865, `amdgpu_mes_init` amdgpu_mes.c:104-260): the EOP buffers,
//! MQDs and rings of both pipes, the write-back slots, a VRAM page per pipe (`resource_1`). BitsOS takes the
//! system pages once (`mesring::lay`, also the kernel queues' memory for `mesq`) and binds them in one bind before
//! the stage's first access, with the register form of the flush on both hubs: MES does not run yet.
//!
//! The stage stands where the chain puts it and asks by state for what is its own: the GART, the secure
//! processor's load of MES's four images with the autoload, the power controller's features, GFXHUB (the bind's
//! flush and MES's own walks need it), the DMAR survey; once in a boot, and not after a run that wrote registers.

use super::guard::{Allow, Guard, HostAsk};
use super::mescmd::{must, told};
use super::mesring::{self, Place, Port, Slots, lay};
use super::nb::Io;
use super::nbregs::NbRegs;
pub(super) use super::mescmd::q_stop;
use super::sdmaq::{QStop, sys_write};
use super::stage::{Ctx, Session, Stage, Stop, read_file, stop};
use super::{fwlist, policy};
use crate::gpucore::fwload::FwSource;
use crate::gpucore::psp::Hw;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use amdgpu_fmt::wb::WbPool;
use core::ops::Range;
use rdna4_core::mes12::{self, Pipe};
use rdna4_core::mes12api::{self as api, HwRes, Legacy, Misc};
use rdna4_core::mqd12::{self, HqdFromMqd, Prop};
use rdna4_core::pte12::PAGE;
use rdna4_core::regmap::Reg;

/// MES after a passed stage `mes`: `mes.ring[0].sched.ready`.
#[derive(Debug)]
pub struct Up {
    pub place: Place,
    /// The scheduler pipe's ring, then the KIQ pipe's.
    pub ports: [Port; 2],
    pub pool: WbPool,
    /// The kernel queues' ring slots, in `mes12::KERNEL_QUEUES`' order (taken with MES's, as Linux's rings are in
    /// `sw_init`).
    pub kq_slots: [Slots; 5],
    /// What CP_MES_GP3_LO read on each pipe (`sched_version`, `kiq_version`).
    pub sched_version: u32,
    pub kiq_version: u32,
    pub cleaner_shader: bool,
}

/// MES in this boot.
#[derive(Debug, Default)]
pub struct State {
    /// The piece of system memory (`mesring::lay::PIECE`), kept for the boot once taken.
    pub piece: Option<u64>,
    /// `resource_1` of both pipes (VRAM offsets: pipe 0's page, then pipe 1's), kept for the boot once taken.
    pub resource1: Option<Range<u64>>,
    /// A register of MES was written in this boot.
    pub tried: bool,
    /// The Reload hook's word (`livemes`, `mesoff::Written`), set together with `tried`.
    pub reload: Option<super::mesoff::Written>,
    pub up: Option<Up>,
    /// The stage `mesq` wrote a register in this boot, and whether it passed (the five kernel queues mapped).
    pub kq_tried: bool,
    pub kq: bool,
    /// Where the KIQ pipe's queue lies in the GART, once the stage bound MES's piece (`messeq`'s rules hold the
    /// queue's four addresses to exactly these objects).
    pub kiq: Option<super::messeq::Kiq>,
}

impl State {
    pub const fn new() -> State {
        State { piece: None, resource1: None, tried: false, reload: None, up: None, kq_tried: false, kq: false, kiq: None }
    }

    pub fn ready(&self) -> bool {
        self.up.is_some()
    }

    pub fn describe(&self) -> String {
        match (&self.up, self.tried) {
            (Some(u), _) => format!(
                "mes on (rings at mc {:#x} and {:#x}, versions {:#x} and {:#x}, {} commands on the scheduler ring and {} on the KIQ ring; kernel queues {}; a bind flushes through MES)",
                u.ports[0].ring_mc(&u.place),
                u.ports[1].ring_mc(&u.place),
                u.sched_version,
                u.kiq_version,
                u.ports[0].seq,
                u.ports[1].seq,
                if self.kq { "mapped" } else if self.kq_tried { "stopped half-way" } else { "not mapped" }
            ),
            (None, true) => String::from("mes stopped half-way (its registers were written; not again in this boot)"),
            (None, false) => String::from("mes not run"),
        }
    }
}

pub fn is_mine(stage: Stage) -> bool {
    matches!(stage, Stage::Mes | Stage::MesQ)
}

/// The stage's own order check: once in a boot, not after a run that wrote registers.
pub fn order(s: &State) -> Result<(), Stop> {
    if s.up.is_some() {
        return Err(stop("der Planer der Karte läuft schon", "mes ran in this boot: a second run is refused"));
    }
    if s.tried {
        return Err(stop("der Planer der Karte wurde in diesem Start schon angefasst und blieb stehen: erst neu starten", "mes stopped half-way in this boot after it wrote registers: the pipes are in no state of Linux's cold start"));
    }
    Ok(())
}

/// Asked by `fw` before it loads anything a second time in one boot: the SMU's firmware and the autoload would
/// start the graphics block anew under a MES that may run (Linux stops MES in `mes_v12_0_kiq_hw_fini` before a
/// resume loads the firmware again; that stop and second start in one boot is not built).
pub fn before_fw(s: &State) -> Result<(), Stop> {
    if s.tried {
        return Err(stop(
            "der Planer der Karte wurde in diesem Start schon gestartet: erst neu starten",
            "mes wrote its registers in this boot: a second fw would load the SMU's firmware and send the autoload under a MES that may run (Linux halts it first, mes_v12_0_kiq_hw_fini); Reload first",
        ));
    }
    Ok(())
}

/// The registers the stage `mes` writes: the selection, MES's control and program counter, the KIQ queue's HQD
/// registers, the unmapped and aggregated doorbells.
pub fn mes_regs() -> Vec<Reg> {
    let mut v = alloc::vec![mes12::GRBM_GFX_CNTL, mes12::CP_MES_CNTL, mes12::CP_MES_PRGRM_CNTR_START, mes12::CP_MES_PRGRM_CNTR_START_HI, mes12::CP_UNMAPPED_DOORBELL];
    v.extend(mes12::HQD);
    v.extend((0..5).map(mes12::doorbell_control));
    v
}

/// What the block's two stages may write: `mes` its registers, the bind's flush (both hubs by registers: MES
/// does not run yet) and the GART table; `mesq` the selection and the HDP flush, nothing else (its commands are
/// memory and a doorbell).
pub fn allow(stage: Stage, s: &Session, n: &NbRegs) -> Allow {
    let mut regs = Vec::new();
    let mut mem = Vec::new();
    match stage {
        Stage::Mes => {
            regs.extend(mes_regs().into_iter().filter_map(|r| n.byte(r)));
            regs.extend(s.flush_regs(n).map(|f| f.written()).unwrap_or_default());
            mem.extend(s.nb.gart.as_ref().map(|g| g.table_mc()));
        }
        Stage::MesQ => {
            regs.extend(n.byte(mes12::GRBM_GFX_CNTL));
            regs.push(s.nb.hdp_flush(n.hdp_flush));
        }
        _ => {}
    }
    Allow { stage: stage.name(), regs, mem }
}

/// The guard of one of the block's stages: MES's two doorbells and its piece of system memory.
pub fn guarded<H: Hw>(g: Guard<H>, stage: Stage, s: &Session) -> Guard<H> {
    let piece: Vec<Range<u64>> = s.mes.piece.map(|at| at..at + lay::PIECE).into_iter().collect();
    let bells = alloc::vec![Pipe::Sched.doorbell(), Pipe::Kiq.doorbell()];
    match stage {
        Stage::Mes => g.with_host(alloc::vec![HostAsk::Pages]).with_doorbells(bells).with_sys(piece),
        Stage::MesQ => g.with_doorbells(bells).with_sys(piece),
        _ => g,
    }
}

/// What a stage that binds after MES runs needs on its guard for the flush through MES: the KIQ pipe's doorbell
/// and MES's piece (the ring and the slots the flush's command writes).
pub fn flush_extras(s: &Session) -> (Vec<u32>, Vec<Range<u64>>) {
    match (&s.mes.up, s.mes.piece) {
        (Some(_), Some(at)) => (alloc::vec![Pipe::Kiq.doorbell()], alloc::vec![at..at + lay::PIECE]),
        _ => (Vec::new(), Vec::new()),
    }
}

/// The secure processor loaded MES's four images (one file, both pipes, `amdgpu_mes_init_microcode`
/// amdgpu_mes.c:820-846) and the autoload went out: the pipes the stage starts have their program.
fn firmware_loaded(s: &Session) -> bool {
    use fwlist::Id;
    s.autoload && [Id::CpMes, Id::CpMesData, Id::CpMes1, Id::CpMes1Data].iter().all(|id| s.loaded.iter().any(|l| l.0 == *id))
}

/// What the stage reads of the stick's files, by their documented headers only.
struct Files {
    start: (u32, u32),
    /// ME, PFP, MEC `ucode_version`.
    gfx: [u32; 3],
}

fn read_files(c: &Ctx, files: &mut dyn FwSource) -> Result<Files, Stop> {
    let mut one = |role| -> Result<Vec<u8>, Stop> {
        let plain = fwlist::file_of(role).ok_or_else(|| stop("eine Datei fehlt auf dem Stick", format!("no file for {role:?} in the card's list")))?;
        read_file(files, fwlist::name_for(plain, c.facts.device, c.facts.revision))
    };
    let bad = |e| stop("eine Datei auf dem Stick passt nicht", format!("{e:?}"));
    let mes = one(fwlist::Role::Mes)?;
    let (lo, hi) = fwlist::mes_start(&mes).map_err(bad)?;
    let mut gfx = [0; 3];
    for (i, role) in [fwlist::Role::Me, fwlist::Role::Pfp, fwlist::Role::Mec].into_iter().enumerate() {
        gfx[i] = fwlist::ucode_version(&one(role)?).map_err(bad)?;
    }
    Ok(Files { start: mes12::pc_start(lo, hi), gfx })
}

/// `adev->reg_offset[hw][0][0..5]`: the discovery table's first five base segments of instance 0.
fn bases(c: &Ctx, hw: u16) -> [u32; 5] {
    core::array::from_fn(|i| c.att.discovery.reg(hw, 0, i, 0).unwrap_or(0))
}

/// After a stop of `mes` or `mesq` in a call that reached the card: GRBM_GFX_CNTL back to 0, so no look or stage
/// after it works under a MES pipe's or a kernel queue's selection (a stop can come between a selection and
/// Linux's `soc24_grbm_select(adev, 0, 0, 0, 0)`; Linux's error paths leave it, as they unwind the whole driver).
/// It comes after the stage's own work and is outside the comparison with Linux's trace.
pub fn after_stop<H: Hw>(hw: &mut H, n: &NbRegs, out: &mut Vec<String>) {
    let Some(b) = n.byte(mes12::GRBM_GFX_CNTL) else { return };
    let back = hw.reg_write(b, 0);
    out.push(format!("mes: GRBM_GFX_CNTL put back to 0 after the stop{}", if back { "" } else { ": the write did not arrive" }));
}

pub fn run(s: &mut Session, stage: Stage, c: &Ctx, io: &mut Io, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    match stage {
        Stage::Mes => start(s, c, io, files, out),
        _ => super::meskq::run(s, io, out),
    }
}

fn wr(io: &mut Io, r: Reg, v: u32) -> Result<(), Stop> {
    io.write(r, v)
}

fn select(io: &mut Io, v: u32) -> Result<(), Stop> {
    wr(io, mes12::GRBM_GFX_CNTL, v)
}

fn mes_prop(p: &Place, port: &Port, pipe: Pipe) -> Prop {
    let i = pipe.index();
    Prop { mqd: p.mc + lay::mqd(i), ring: p.mc + lay::ring(i), ring_bytes: mes12::RING_BYTES, rptr: p.slot_mc(port.slots.rptr), wptr: p.slot_mc(port.slots.wptr), eop: p.mc + lay::eop(i), doorbell: pipe.doorbell(), high_priority: false }
}

fn put_mqd(io: &mut Io, at: u64, m: &[u32]) -> Result<(), Stop> {
    let bytes: Vec<u8> = m.iter().flat_map(|w| w.to_le_bytes()).collect();
    sys_write(io.card, at, &bytes).map_err(q_stop)
}

fn gp3(io: &mut Io, pipe: Pipe) -> Result<u32, Stop> {
    select(io, mes12::mes_select(pipe))?;
    let v = io.read(mes12::CP_MES_GP3_LO)?;
    select(io, 0)?;
    Ok(v)
}

fn start(s: &mut Session, c: &Ctx, io: &mut Io, files: &mut dyn FwSource, out: &mut Vec<String>) -> Result<(), Stop> {
    order(&s.mes)?;
    if s.nb.gart.is_none() {
        return Err(stop("zuerst den Speicher der Karte einrichten", "no GART: MES's rings, MQDs and slots lie behind it"));
    }
    if !firmware_loaded(s) {
        return Err(stop("zuerst die Steuerprogramme laden", "the secure processor has not loaded MES's four images (LOAD_IP_FW types 33, 34, 81, 82) and sent the autoload in this boot"));
    }
    if !s.pmfw.gc_powered() {
        return Err(stop("zuerst die Energieverwaltung der Karte starten", "the power controller's features are not on in this boot (EnableAllSmuFeatures was not answered): no register of MES is read or written before it"));
    }
    if !s.gc.hub {
        return Err(stop("zuerst den Speicher des Grafikteils einrichten", "GFXHUB is not on: MES reads its rings and MQDs through it, and the bind's flush walks it"));
    }
    let f = read_files(c, files)?;
    let survey = io.card.dma_survey();
    policy::may_send(io.driven, survey).map_err(|why| stop("der Rechner lässt die Karte nicht an den Arbeitsspeicher", why))?;
    let sys = match s.mes.piece {
        Some(sys) => sys,
        None => io.card.pages(lay::PIECE).ok_or_else(|| stop("kein Arbeitsspeicher für die Karte frei", "no pages for MES"))?,
    };
    s.mes.piece = Some(sys);
    if s.mes.resource1.is_none() {
        let bump = s.bump.as_mut().ok_or_else(|| stop("zuerst den Speicher der Karte einrichten", "no VRAM manager"))?;
        s.mes.resource1 = Some(bump.take("MES resource_1 pages", 2 * PAGE, PAGE).map_err(|e| stop("kein Platz im Speicher der Karte", format!("{e:?}")))?);
    }
    let r1 = s.mes.resource1.clone().ok_or_else(|| stop("Reihenfolge stimmt nicht", "no resource_1"))?;
    let l = s.layout.as_ref().ok_or_else(|| stop("zuerst den Speicher der Karte einrichten", "no layout"))?;
    let resource1 = [l.mc(r1.start), l.mc(r1.start + PAGE)];
    // What Linux's sw_init left: the write-back slots in `amdgpu_mes_init`'s and `amdgpu_ring_init`'s order, the
    // EOP buffers and MQDs cleared, every ring's slots as `amdgpu_ring_init` sets them.
    let mut pool = WbPool::new(mesring::WB_SLOTS);
    let mut take = || pool.get().map_err(|e| q_stop(QStop::Slots(e)));
    let (sch_ctx0, query0, sch_ctx1, query1) = (take()?, take()?, take()?, take()?);
    let (sch_ctx, query) = ([sch_ctx0, sch_ctx1], [query0, query1]);
    let mut ports = [Port::new(Pipe::Sched, &mut pool).map_err(q_stop)?, Port::new(Pipe::Kiq, &mut pool).map_err(q_stop)?];
    let mut kq: Vec<Slots> = Vec::new();
    for _ in mes12::KERNEL_QUEUES {
        kq.push(Slots::take(&mut pool).map_err(q_stop)?);
    }
    let kq_slots: [Slots; 5] = kq.try_into().map_err(|_| stop("Reihenfolge stimmt nicht", "five kernel queues"))?;
    let cpu = Place { sys, mc: 0 };
    sys_write(io.card, sys, &alloc::vec![0u8; lay::PIECE as usize]).map_err(q_stop)?;
    for sl in ports.iter().map(|p| p.slots).chain(kq_slots) {
        sl.init(io.card, &cpu).map_err(q_stop)?;
    }
    out.push(format!("mes: system memory at {sys:#x} ({} bytes: both pipes' MQD, EOP and ring of {} bytes, the write-back page, the kernel queues' MQDs, EOP buffers and rings); resource_1 in vram {:#x}..{:#x}; the pipes start at {:#x} (high {:#x}) from the file's header", lay::PIECE, mes12::RING_BYTES, r1.start, r1.end, f.start.0, f.start.1));
    s.mes.tried = true;
    if let Some(hook) = &s.mes.reload {
        hook.set();
    }
    // The one bind, right before the stage's first access: MES does not run yet, so the flush walks both hubs by
    // registers.
    let flush = io.flush()?;
    let gart = s.nb.gart.as_mut().ok_or_else(|| stop("Reihenfolge stimmt nicht", "no GART"))?;
    let mut lines = Vec::new();
    let mc = gart.map(io.card.hw(), &flush, "MES rings, MQDs and the kernel queues", &super::nb::pages_of(sys, lay::PIECE), &mut lines).map_err(|e| stop("der Speicher für den Planer lässt sich nicht einbinden", format!("{e:?}")))?;
    out.extend(lines);
    let p = Place { sys, mc };
    s.mes.kiq = Some(super::messeq::Kiq::of(&p, &ports[1]));
    // mes_v12_0_enable(adev, true): one pipe after the other under its selection, then the settle.
    for pipe in Pipe::BOTH {
        select(io, mes12::mes_select(pipe))?;
        for op in mes12::enable(pipe, f.start) {
            io.op(op)?;
        }
    }
    select(io, 0)?;
    io.card.hw().delay_us(mes12::ENABLE_SETTLE_US);
    // mes_v12_0_queue_init(KIQ): the MQD in memory, the HQD registers from it, the version.
    let kiq_mqd = mqd12::mes(&mes_prop(&p, &ports[1], Pipe::Kiq));
    put_mqd(io, sys + lay::mqd(1), &kiq_mqd)?;
    let h = HqdFromMqd::of(&kiq_mqd).ok_or_else(|| stop("Reihenfolge stimmt nicht", "no MQD"))?;
    select(io, mes12::mes_select(Pipe::Kiq))?;
    for op in mes12::queue_init_register(&h).into_iter().chain(mes12::queue_init_tail(&h)) {
        io.op(op)?;
    }
    select(io, 0)?;
    let kiq_version = gp3(io, Pipe::Kiq)?;
    let res = |pipe: Pipe, version: u32| HwRes {
        sched: pipe == Pipe::Sched,
        sch_ctx: p.slot_mc(sch_ctx[pipe.index()]),
        query_fence: p.slot_mc(query[pipe.index()]),
        gc_base: bases(c, crate::gpucore::hwid::GC),
        mmhub_base: bases(c, crate::gpucore::hwid::MMHUB),
        osssys_base: bases(c, crate::gpucore::hwid::OSSSYS),
        oversubscription_timer: mes12::oversubscription_timer(mes12::fw_version(version)),
    };
    let [sched_port, kiq_port] = &mut ports;
    must(kiq_port, io, &p, &mut pool, api::set_hw_resources(&res(Pipe::Kiq, kiq_version)), "der Planer der Karte antwortet nicht", out)?;
    told(kiq_port, io, &p, &mut pool, api::set_hw_resources_1(resource1[1]), out)?;
    // mes_v12_0_hw_init.
    io.op(rdna4_core::hubseq::Op::Modify(mes12::CP_UNMAPPED_DOORBELL, mes12::unmapped_doorbell))?;
    let sched_mqd = mqd12::mes(&mes_prop(&p, sched_port, Pipe::Sched));
    put_mqd(io, sys + lay::mqd(0), &sched_mqd)?;
    let schq = Legacy { queue_type: api::qtype::SCHQ, pipe: 0, queue: 0, doorbell: Pipe::Sched.doorbell(), mqd: p.mc + lay::mqd(0), wptr: p.slot_mc(sched_port.slots.wptr) };
    must(kiq_port, io, &p, &mut pool, api::add_legacy(&schq), "der Planer der Karte antwortet nicht", out)?;
    let sched_version = gp3(io, Pipe::Sched)?;
    must(sched_port, io, &p, &mut pool, api::set_hw_resources(&res(Pipe::Sched, sched_version)), "der Planer der Karte antwortet nicht", out)?;
    if mes12::fw_version(sched_version) >= mes12::HW_RSRC_1_FROM {
        told(sched_port, io, &p, &mut pool, api::set_hw_resources_1(resource1[0]), out)?;
    }
    for (level, doorbell) in mes12::AGGREGATED_DOORBELLS.into_iter().enumerate() {
        let r = mes12::doorbell_control(level as u32);
        let v = io.read(r)?;
        wr(io, r, mes12::aggregated(v, doorbell))?;
    }
    wr(io, mes12::CP_HQD_GFX_CONTROL, mes12::HQD_GFX_CONTROL)?;
    must(sched_port, io, &p, &mut pool, api::query_status(), "der Planer der Karte ist beschäftigt", out).map_err(|Stop(plain, d)| stop(plain, format!("MES is busy: {d}")))?;
    // amdgpu_mes_update_enforce_isolation: CHANGE_CONFIG only with the cleaner shader on, which these files'
    // versions turn on (gfx_v12_0.c:1450-1458). MES's own version is the scheduler pipe's GP3: Linux takes it from
    // a dword of the file's body (`ucode_ptr[24]`, amdgpu_mes.c:813-815), which is no documented header and is
    // not read here.
    let cleaner = mes12::cleaner_shader(f.gfx[0], f.gfx[1], f.gfx[2], mes12::fw_version(sched_version));
    if cleaner {
        must(kiq_port, io, &p, &mut pool, api::misc_frame(Misc::ChangeConfig { limit_single_process: false }), "der Planer der Karte nimmt die Einstellung nicht an", out)?;
    }
    out.push(format!(
        "mes: kiq version {kiq_version:#x}, scheduler version {sched_version:#x} (firmware {:#x}, API {:#x}, the header's API {:#x}); cleaner shader {} (ME {:#x}, PFP {:#x}, MEC {:#x}, MES {:#x}); the file's own version dword (amdgpu_mes_validate_fw_version) is in its body and not read",
        mes12::fw_version(sched_version),
        mes12::api_version(sched_version),
        api::HEADER_API_VERSION,
        if cleaner { "on: CHANGE_CONFIG sent" } else { "off: no CHANGE_CONFIG" },
        f.gfx[0],
        f.gfx[1],
        f.gfx[2],
        mes12::fw_version(sched_version)
    ));
    s.mes.up = Some(Up { place: p, ports, pool, kq_slots, sched_version, kiq_version, cleaner_shader: cleaner });
    Ok(())
}
