//! GX-7k: a fake MES inside the chip fake (`FakeGpu`), good enough for Linux's MES start, the self test, the
//! legacy queue map and the fences: CP_MES_CNTL (halt, pipe resets and actives), the program counters, the
//! firmware versions in CP_MES_GP3_LO (the capture's values), RLC_CP_SCHEDULERS, the HQD/MQD registers banked by
//! GRBM_GFX_CNTL; the KIQ (pipe 1) from its HQD registers, fed by its doorbell, running PM4 (NOP, WRITE_DATA,
//! MAP_QUEUES of the scheduler queue from its MQD, SET_RESOURCES); the scheduler (pipe 0) from the mapped MQD,
//! fed by its doorbell, running the API frames (SET_HW_RESOURCES[_1], QUERY_SCHEDULER_STATUS, MISC READ/WRITE_REG,
//! ADD_QUEUE map_legacy_kq, REMOVE_QUEUE unmap_legacy_queue), each answered with its api_status fence; the mapped
//! GFX and compute queues run by GR-2's PM4 interpreter (`sim::Engine`) through GFXHUB's VMID 0, fed by their
//! doorbells, with the rptr writeback.
//!
//! Assumptions (no register trace of the firmware's inside exists): MES runs at once when its ring moves (the
//! chip is asynchronous; the stage polls either way); the firmware starts only when both program counters hold
//! the header's uc start (`>> 2`), else GP3 reads 0; SET_HW_RESOURCES with masks other than Linux's hangs the
//! scheduler silently (no status, no fence: the roadmap's warning); DEQUEUE_REQUEST takes effect at once; a pipe
//! reset forgets that pipe's queue but keeps its HQD registers (so a later run meets a stale CP_HQD_ACTIVE, as an
//! earlier BitsOS run's Quiesce halt leaves it); a mapped queue runs only while its engine runs (ME/PFP for GFX,
//! MEC ME1 for compute: the caller says).
//!
//! GX-7m: ADD_QUEUE of a process queue (no map_legacy_kq) as the firmware is documented to handle it (no trace
//! of its inside exists): the process gets the lowest VMID of SET_HW_RESOURCES' GFXHUB mask no other process
//! holds (one VMID per page table), MES writes that context's page-table base (the root | VALID), the IH LUT entry
//! (the PASID) and an invalidate on `FAKE_MES_ENGINE`, then runs the queue from its MQD with every access of the
//! queue (ring, rptr writeback, IBs, fences) through that VMID; REMOVE_QUEUE by doorbell drops it.
//!
//! GX-7m3: a compute queue whose engine halted at a VM fault (`sdmafake::FakeBus`: CPC has no way to drop it) stays
//! halted with its rptr where it stopped until it is removed; the kernel ring's HQD reads active while mapped
//! (ME 1, its pipe and queue) and off after REMOVE_QUEUE, or after DEQUEUE_REQUEST 2 and SPI_COMPUTE_QUEUE_RESET.

use super::mes::MesAddrs;
use crate::ipmod::gfx11::R;
use crate::ipmod::mes11::{self, HwMasks, M, f};
use crate::ipmod::mesapi::{self, at};
use crate::ipmod::mesmqd::{c, g};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use amdgpu_fmt::bus::GpuBus;
use amdgpu_fmt::pm4;
use amdgpu_fmt::sim::{Engine, Event, Isa, Limits, Stop};

/// The capture's CP_MES_GP3_LO of the scheduler and the KIQ pipe, and RLC_CP_SCHEDULERS' first read.
pub const VERSIONS: [u32; 2] = [0x0102_508b, 0x0603_8110];
pub const RLC_COLD: u32 = 0x3038;
/// The program counter the firmware needs (the 780M's headers: uc start 0xffff_ffff_f000_5000).
pub const UC_START: u64 = 0xffff_ffff_f000_5000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MesHostile {
    None,
    /// The pipes never start their firmware (GP3 reads 0).
    NoFirmware,
    /// The scheduler firmware is too old for legacy queues (0x40).
    OldFirmware,
    /// The KIQ never runs its ring.
    KiqDead,
    /// The scheduler never answers a frame.
    SchedDead,
    /// The scheduler answers the fence frame but writes no api_status.
    NoApiStatus,
    /// ADD_QUEUE is never answered (the map hangs).
    MapRefused,
    /// The mapped queues never fetch their rings.
    QueuesDead,
    /// MISC WRITE_REG is lost (the self test fails).
    MiscLost,
    /// The mapped queues never fetch their rings and REMOVE_QUEUE wedges the scheduler (nothing answered after).
    UnmapHangs,
    /// GX-7m3: ADD_QUEUE of a process queue is not answered (legacy maps still are).
    ProcRefused,
    /// GX-7m3: REMOVE_QUEUE wedges the scheduler; the queues still run.
    RemoveHangs,
    /// GX-7m3: REMOVE_QUEUE answers and leaves the legacy HQD active: it needs the reset dequeue.
    UnmapLeavesActive,
    /// GX-7m3: as `UnmapLeavesActive`, and the reset dequeue does nothing either.
    HqdStuck,
    /// GX-7m3: a legacy queue mapped again never runs (a halt the per-queue reset does not reach).
    RemapDead,
    /// GX-7m3: the process queue's first ADD_QUEUE is answered, the next is not.
    ProcAgainRefused,
    /// GX-9a: CP_MES_CNTL does not take a halt: MES goes on running.
    HaltIgnored,
}

/// CP_HQD_ERROR.SUA_ERROR (gc_11_0_0_sh_mask.h:17716): an IB address that is not sign-extended (`Hang::NotCanonicalIb`).
pub const HQD_ERROR_SUA: u32 = 0x10;

/// A queue a MES pipe runs (from its HQD registers or its MQD).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Pipe {
    base: u64,
    ring_dw: u64,
    rptr: u64,
    doorbell: u32,
}

/// A legacy queue MES mapped (or, GX-7m, a process queue: `vmid` != 0).
struct Legacy {
    qtype: u32,
    doorbell: u32,
    rptr_wb: u64,
    wptr: u64,
    engine: Option<Engine>,
    vmid: u8,
    page_table: u64,
}

/// GX-7m: the invalidation engine the fake MES flushes its VMIDs on (the firmware's own choice is not visible to
/// the driver; not 17, the CPU's, and not a kernel ring's 0, 1 or 12).
pub const FAKE_MES_ENGINE: u8 = 16;

/// GX-7m: a process queue's accesses: VMID 0 (what GR-2's engine and its IB packets name) becomes the queue's
/// VMID (the HQD's), as CP_HQD_VMID makes every fetch of the queue.
struct InVmid<'a> {
    bus: &'a mut dyn GpuBus,
    vmid: u8,
}

impl GpuBus for InVmid<'_> {
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), amdgpu_fmt::bus::MemFault> {
        self.bus.read(if vmid == 0 { self.vmid } else { vmid }, addr, out)
    }
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), amdgpu_fmt::bus::MemFault> {
        self.bus.write(if vmid == 0 { self.vmid } else { vmid }, addr, data)
    }
    fn fetch(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), amdgpu_fmt::bus::MemFault> {
        self.bus.fetch(if vmid == 0 { self.vmid } else { vmid }, addr, out)
    }
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        self.bus.reg_read(reg)
    }
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        self.bus.reg_write(reg, value)
    }
}

pub struct MesFake {
    pub addrs: Option<MesAddrs>,
    /// GX-7m: GFXHUB and the IH LUT (what MES programs for a process queue).
    vm: Option<super::vmring::VmAddrs>,
    owned: Vec<(M, u32)>,
    grbm_byte: Option<u32>,
    grbm: u32,
    cntl: u32,
    rlc: u32,
    pc: [(u32, u32); 2],
    started: bool,
    banks: BTreeMap<(u32, u32), u32>,
    kiq: Option<Pipe>,
    sched: Option<Pipe>,
    sched_hung: bool,
    kiq_hung: bool,
    legacy: Vec<Legacy>,
    pub hostile: MesHostile,
    /// The API frames run (opcode), the KIQ packets run (opcode), the last SET_HW_RESOURCES frame.
    pub frames: Vec<u32>,
    pub kiq_packets: Vec<u8>,
    pub hw_res: Option<Vec<u32>>,
    pub doorbells: Vec<(u32, u64, bool)>,
    pub events: Vec<Event>,
    pub stops: Vec<Stop>,
    /// CP_MES_CNTL as written.
    pub cntl_writes: Vec<u32>,
    /// Dequeues requested (the GRBM selection).
    pub dequeues: Vec<u32>,
    /// GX-7m3: SPI_COMPUTE_QUEUE_RESET writes (the GRBM selection at the time).
    pub resets: Vec<u32>,
    /// GX-7m3: the doorbells of legacy queues mapped, in order (a queue's second map is a recovery).
    pub maps: Vec<u32>,
    /// GX-7m3: REMOVE_QUEUE frames for a queue this MES does not hold (the chip's answer to one is unknown: never sent).
    pub unknown_removes: u32,
    proc_adds: u32,
    /// GX-7m3: CPC_UTCL1_ERROR: its halt bit latches when a compute queue halts at a VM fault (an assumption about
    /// the chip, whose value the rung prints), written 0 by the recovery.
    pub cpc_error: u32,
    /// GX-7m3: the writes to it.
    pub cpc_error_writes: Vec<u32>,
    /// GX-9a: the writes to SQ_CMD.
    pub sq_cmds: Vec<u32>,
}

/// What a doorbell rang.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rang {
    Kiq,
    Sched,
    Legacy,
}

impl MesFake {
    pub fn new(d: &super::disc::Discovery) -> MesFake {
        let addrs = MesAddrs::from_discovery(d);
        let owned = addrs.map_or(Vec::new(), |a| M::ALL.iter().filter_map(|&r| a.byte(r).map(|b| (r, b))).collect());
        MesFake {
            vm: super::vmring::VmAddrs::from_discovery(d),
            grbm_byte: addrs.and_then(|a| a.gfx(R::GrbmGfxCntl)),
            addrs,
            owned,
            grbm: 0,
            cntl: f::MES_HALT,
            rlc: RLC_COLD,
            pc: [(0, 0); 2],
            started: false,
            banks: BTreeMap::new(),
            kiq: None,
            sched: None,
            sched_hung: false,
            kiq_hung: false,
            legacy: Vec::new(),
            hostile: MesHostile::None,
            frames: Vec::new(),
            kiq_packets: Vec::new(),
            hw_res: None,
            doorbells: Vec::new(),
            events: Vec::new(),
            stops: Vec::new(),
            cntl_writes: Vec::new(),
            dequeues: Vec::new(),
            resets: Vec::new(),
            maps: Vec::new(),
            unknown_removes: 0,
            proc_adds: 0,
            cpc_error: 0,
            cpc_error_writes: Vec::new(),
            sq_cmds: Vec::new(),
        }
    }

    fn which(&self, byte: u32) -> Option<M> {
        self.owned.iter().find(|x| x.1 == byte).map(|x| x.0)
    }

    pub fn owns(&self, byte: u32) -> bool {
        self.which(byte).is_some()
    }

    pub fn is_grbm(&self, byte: u32) -> bool {
        self.grbm_byte == Some(byte)
    }

    /// GRBM_GFX_CNTL as written (the GC fake keeps the register; this fake follows the selection).
    pub fn grbm_write(&mut self, v: u32) {
        self.grbm = v;
    }

    fn sel(&self) -> u32 {
        self.grbm & !(0xf << f::GRBM_VMID_SHIFT)
    }

    fn pipe_sel(pipe: u32) -> u32 {
        mes11::grbm_select(mes11::MES_ME, pipe, 0, 0)
    }

    /// GX-7m3: the kernel compute ring's HQD as GRBM_GFX_CNTL selects it.
    fn compute_sel() -> u32 {
        mes11::grbm_select(mes11::COMPUTE_ME, mes11::COMPUTE_PIPE, mes11::COMPUTE_QUEUE, 0)
    }

    fn gfx_sel() -> u32 {
        mes11::grbm_select(0, mes11::GFX_PIPE, mes11::GFX_QUEUE, 0)
    }

    /// GX-9a: `byte` is SQ_CMD.
    pub fn byte_of_sq(&self, byte: u32) -> bool {
        self.addrs.and_then(|a| a.byte(M::SqCmd)) == Some(byte)
    }

    pub fn running(&self) -> bool {
        mes11::cntl_running(self.cntl) && self.started
    }

    pub fn cntl(&self) -> u32 {
        self.cntl
    }

    /// The state Linux's load leaves: MES runs (both pipes), the KIQ's and the scheduler's HQDs active.
    pub fn as_linux_left(&mut self) {
        self.cntl = mes11::CNTL_RUN;
        self.started = true;
        for p in mes11::PIPES {
            self.set_bank(Self::pipe_sel(p), M::CpHqdActive, 1);
        }
    }

    /// The kernel compute ring's HQD register `r` (tests).
    pub fn compute_hqd(&self, r: M) -> u32 {
        self.bank(Self::compute_sel(), r)
    }

    /// The HQD registers of `pipe` as `r` reads (tests).
    pub fn hqd(&self, pipe: u32, r: M) -> u32 {
        self.bank(Self::pipe_sel(pipe), r)
    }

    fn bank(&self, sel: u32, r: M) -> u32 {
        self.addrs.and_then(|a| a.byte(r)).and_then(|b| self.banks.get(&(sel, b)).copied()).unwrap_or(0)
    }

    fn set_bank(&mut self, sel: u32, r: M, v: u32) {
        if let Some(b) = self.addrs.and_then(|a| a.byte(r)) {
            self.banks.insert((sel, b), v);
        }
    }

    pub fn read(&self, byte: u32) -> u32 {
        let sel = self.sel();
        match self.which(byte) {
            Some(M::CpMesCntl) => self.cntl,
            Some(M::RlcCpSchedulers) => self.rlc,
            Some(M::CpMesGp3Lo) => {
                let pipe = sel & 3;
                let me = (sel >> f::GRBM_MEID_SHIFT) & 3;
                if me != mes11::MES_ME || !self.started || self.hostile == MesHostile::NoFirmware {
                    return 0;
                }
                let v = VERSIONS.get(pipe as usize).copied().unwrap_or(0);
                if pipe == 0 && self.hostile == MesHostile::OldFirmware { (v & !mes11::VERSION_MASK) | 0x40 } else { v }
            }
            Some(M::CpMesPrgrmCntrStart) => self.pc.get((sel & 3) as usize).map_or(0, |p| p.0),
            Some(M::CpMesPrgrmCntrStartHi) => self.pc.get((sel & 3) as usize).map_or(0, |p| p.1),
            Some(M::CpcUtcl1Error) => self.cpc_error,
            Some(M::CpMqdControl) if !self.banks.contains_key(&(sel, byte)) => mes11::dflt::CP_MQD_CONTROL,
            Some(_) => self.banks.get(&(sel, byte)).copied().unwrap_or(0),
            None => 0,
        }
    }

    /// A register write.
    pub fn write(&mut self, byte: u32, v: u32) {
        let Some(r) = self.which(byte) else { return };
        let sel = self.sel();
        match r {
            M::CpMesCntl => {
                self.cntl_writes.push(v);
                if self.hostile == MesHostile::HaltIgnored && v & f::MES_HALT != 0 {
                    return;
                }
                if v & f::MES_PIPE0_RESET != 0 {
                    self.sched = None;
                    self.sched_hung = false;
                }
                if v & f::MES_PIPE1_RESET != 0 {
                    self.kiq = None;
                    self.kiq_hung = false;
                }
                if v & f::MES_HALT != 0 || v & (f::MES_PIPE0_ACTIVE | f::MES_PIPE1_ACTIVE) == 0 {
                    self.started = false;
                } else if !self.started {
                    let want = mes11::prgrm_cntr(UC_START);
                    self.started = self.pc.iter().all(|&p| p == want) && self.hostile != MesHostile::NoFirmware;
                }
                self.cntl = v;
            }
            M::RlcCpSchedulers => self.rlc = v,
            M::CpMesGp3Lo => {}
            M::CpMesPrgrmCntrStart | M::CpMesPrgrmCntrStartHi => {
                if let Some(p) = self.pc.get_mut((sel & 3) as usize) {
                    if r == M::CpMesPrgrmCntrStart {
                        p.0 = v;
                    } else {
                        p.1 = v;
                    }
                }
            }
            M::CpcUtcl1Error => {
                self.cpc_error = v;
                self.cpc_error_writes.push(v);
            }
            M::SqCmd => self.sq_cmds.push(v),
            M::CpHqdDequeueRequest if v == f::DEQUEUE_RESET => {
                self.banks.insert((sel, byte), v);
            }
            M::SpiComputeQueueReset => {
                self.resets.push(sel);
                if self.bank(sel, M::CpHqdDequeueRequest) == f::DEQUEUE_RESET && self.hostile != MesHostile::HqdStuck {
                    self.set_bank(sel, M::CpHqdActive, 0);
                }
                if self.bank(sel, M::CpGfxHqdDequeueRequest) == f::DEQUEUE_RESET && self.hostile != MesHostile::HqdStuck {
                    self.set_bank(sel, M::CpGfxHqdActive, 0);
                }
            }
            M::CpHqdDequeueRequest => {
                if v & f::DEQUEUE_REQ != 0 {
                    self.dequeues.push(sel);
                    self.set_bank(sel, M::CpHqdActive, 0);
                    if sel == Self::pipe_sel(mes11::KIQ_PIPE) {
                        self.kiq = None;
                    } else if sel == Self::pipe_sel(mes11::SCHED_PIPE) {
                        self.sched = None;
                    }
                }
            }
            M::CpHqdActive => {
                self.banks.insert((sel, byte), v);
                if sel == Self::pipe_sel(mes11::KIQ_PIPE) && v & f::HQD_ACTIVE != 0 {
                    let base = (u64::from(self.bank(sel, M::CpHqdPqBaseHi)) << 32 | u64::from(self.bank(sel, M::CpHqdPqBase))) << 8;
                    let qs = self.bank(sel, M::CpHqdPqControl) & f::PQ_QUEUE_SIZE_MASK;
                    let db = self.bank(sel, M::CpHqdPqDoorbellControl);
                    let doorbell = if db & f::DB_EN != 0 { (db & f::DB_OFFSET_MASK) >> 2 } else { u32::MAX };
                    self.kiq = Some(Pipe { base, ring_dw: 1u64 << (qs + 1).min(30), rptr: u64::from(self.bank(sel, M::CpHqdPqRptr)), doorbell });
                }
            }
            _ => {
                self.banks.insert((sel, byte), v);
            }
        }
    }

    /// A doorbell write: which ring it moved (`None`: nobody listens; dropped as on the chip).
    pub fn doorbell(&mut self, index_dw: u32, value: u64, aper: bool) -> Option<Rang> {
        let r = if !aper || !self.running() {
            None
        } else if self.kiq.is_some_and(|k| k.doorbell == index_dw) {
            Some(Rang::Kiq)
        } else if self.sched.is_some_and(|s| s.doorbell == index_dw) {
            Some(Rang::Sched)
        } else if let Some(q) = self.legacy.iter_mut().find(|q| q.doorbell == index_dw) {
            q.wptr = value;
            Some(Rang::Legacy)
        } else {
            None
        };
        self.doorbells.push((index_dw, value, r.is_some()));
        r
    }

    /// Whether MES's KIQ, scheduler or a mapped queue listens on `index_dw` (the FakeGpu sends it here then).
    pub fn has_doorbell(&self, index_dw: u32) -> bool {
        self.running() && (self.kiq.is_some_and(|k| k.doorbell == index_dw) || self.sched.is_some_and(|s| s.doorbell == index_dw) || self.legacy.iter().any(|q| q.doorbell == index_dw))
    }

    /// Runs what the doorbell moved: the KIQ's or the scheduler's ring up to `value`, or the legacy queues
    /// (`gfx_runs`/`mec_runs`: their engines run).
    pub fn kick(&mut self, bus: &mut dyn GpuBus, what: Rang, value: u64, gfx_runs: bool, mec_runs: bool) {
        match what {
            Rang::Kiq => self.run_kiq(bus, value),
            Rang::Sched => self.run_sched(bus, value),
            Rang::Legacy => self.run_legacy(bus, gfx_runs, mec_runs),
        }
    }

    fn rd(bus: &mut dyn GpuBus, addr: u64) -> u32 {
        let mut b = [0u8; 4];
        bus.read(0, addr, &mut b).map_or(0, |_| u32::from_le_bytes(b))
    }

    fn rd_mqd(bus: &mut dyn GpuBus, addr: u64) -> Vec<u32> {
        (0..mes11::MQD_DW as u64).map(|i| Self::rd(bus, addr + i * 4)).collect()
    }

    fn run_kiq(&mut self, bus: &mut dyn GpuBus, wptr: u64) {
        let Some(mut k) = self.kiq else { return };
        if self.hostile == MesHostile::KiqDead || self.kiq_hung {
            return;
        }
        let mask = k.ring_dw - 1;
        let mut budget = 1u64 << 18;
        while k.rptr != wptr && budget > 0 {
            let base = k.base;
            let rp = k.rptr;
            let at = |i: u64| base + ((rp + i) & mask) * 4;
            let h = Self::rd(bus, at(0));
            if h == pm4::PAD || h >> 30 == 2 {
                k.rptr += 1;
                budget -= 1;
                continue;
            }
            if h >> 30 != 3 {
                self.kiq_hung = true;
                break;
            }
            let op = ((h >> 8) & 0xff) as u8;
            let n = u64::from((h >> 16) & 0x3fff) + 2;
            let d: Vec<u32> = (1..n).map(|i| Self::rd(bus, at(i))).collect();
            self.kiq_packets.push(op);
            match op {
                pm4::op::NOP | pm4::op::SET_RESOURCES => {}
                pm4::op::WRITE_DATA if d.len() >= 4 => {
                    let dst = (d[0] >> 8) & 0xf;
                    let addr = u64::from(d[1]) | u64::from(d[2]) << 32;
                    let one = d[0] & pm4::WR_ONE_ADDR != 0;
                    for (i, &v) in d[3..].iter().enumerate() {
                        if dst == pm4::dst_sel::REG {
                            let _ = bus.reg_write(addr as u32 + if one { 0 } else { i as u32 }, v);
                        } else {
                            let _ = bus.write(0, addr + i as u64 * 4, &v.to_le_bytes());
                        }
                    }
                }
                pm4::op::MAP_QUEUES if d.len() >= 6 => {
                    let (me, eng) = ((d[0] >> 18) & 3, (d[0] >> 26) & 7);
                    let db = (d[1] >> 2) & 0x3ff_ffff;
                    let mqd = u64::from(d[2]) | u64::from(d[3]) << 32;
                    if me == mesapi::MAP_ME_MES && eng == mesapi::MAP_ENG_SEL_MES {
                        let m = Self::rd_mqd(bus, mqd);
                        let base = (u64::from(m[c::PQ_BASE_HI]) << 32 | u64::from(m[c::PQ_BASE_LO])) << 8;
                        let qs = m[c::PQ_CONTROL] & f::PQ_QUEUE_SIZE_MASK;
                        self.sched = Some(Pipe { base, ring_dw: 1u64 << (qs + 1).min(30), rptr: 0, doorbell: db });
                        let sel = Self::pipe_sel(mes11::SCHED_PIPE);
                        self.set_bank(sel, M::CpHqdActive, 1);
                        self.set_bank(sel, M::CpMqdBaseAddr, m[c::MQD_BASE_LO]);
                        self.set_bank(sel, M::CpHqdPqDoorbellControl, m[c::DOORBELL_CONTROL]);
                    }
                }
                _ => {
                    self.kiq_hung = true;
                    break;
                }
            }
            k.rptr += n;
            budget = budget.saturating_sub(n);
        }
        if let Some(x) = self.kiq.as_mut() {
            x.rptr = k.rptr;
        }
    }

    fn run_sched(&mut self, bus: &mut dyn GpuBus, wptr: u64) {
        let Some(mut s) = self.sched else { return };
        if self.hostile == MesHostile::SchedDead || self.sched_hung {
            return;
        }
        let mask = s.ring_dw - 1;
        let mut budget = 1024;
        while s.rptr < wptr && budget > 0 {
            budget -= 1;
            let d: Vec<u32> = (0..mesapi::FRAME_DW as u64).map(|i| Self::rd(bus, s.base + ((s.rptr + i) & mask) * 4)).collect();
            let (ty, op, size) = mesapi::header_fields(d[0]);
            if ty != mesapi::TYPE_SCHEDULER || size as usize != mesapi::FRAME_DW {
                self.sched_hung = true;
                break;
            }
            self.frames.push(op);
            let answered = match op {
                mesapi::op::SET_HW_RSRC => {
                    self.hw_res = Some(d.clone());
                    let m = HwMasks::LINUX_780M;
                    let ok = d[at::HW_VMID_MMHUB] == m.vmid_mmhub
                        && d[at::HW_VMID_GFXHUB] == m.vmid_gfxhub
                        && d[at::HW_COMPUTE_HQD..at::HW_COMPUTE_HQD + 8] == m.compute_hqd
                        && d[at::HW_GFX_HQD..at::HW_GFX_HQD + 2] == m.gfx_hqd
                        && d[at::HW_SDMA_HQD..at::HW_SDMA_HQD + 2] == m.sdma_hqd;
                    if !ok {
                        // Wrong masks: the scheduler hangs silently.
                        self.sched_hung = true;
                        break;
                    }
                    true
                }
                mesapi::op::SET_HW_RSRC_1 | mesapi::op::QUERY_SCHEDULER_STATUS => true,
                mesapi::op::MISC => {
                    let reg = d[at::MISC_DATA];
                    match d[at::MISC_OPCODE] {
                        mesapi::misc::READ_REG => {
                            let v = bus.reg_read(reg).unwrap_or(0);
                            let _ = bus.write(0, mesapi::get64(&d, at::MISC_DATA + 1), &v.to_le_bytes());
                        }
                        mesapi::misc::WRITE_REG if self.hostile != MesHostile::MiscLost => {
                            let _ = bus.reg_write(reg, d[at::MISC_DATA + 1]);
                        }
                        _ => {}
                    }
                    true
                }
                mesapi::op::ADD_QUEUE if self.hostile == MesHostile::MapRefused => false,
                mesapi::op::ADD_QUEUE if self.hostile == MesHostile::ProcRefused && d[at::ADD_FLAGS] & mesapi::ADD_MAP_LEGACY_KQ == 0 => false,
                mesapi::op::ADD_QUEUE if self.hostile == MesHostile::ProcAgainRefused && self.proc_adds > 0 && d[at::ADD_FLAGS] & mesapi::ADD_MAP_LEGACY_KQ == 0 => false,
                mesapi::op::ADD_QUEUE => {
                    if d[at::ADD_FLAGS] & mesapi::ADD_MAP_LEGACY_KQ != 0 {
                        self.map_legacy(bus, &d);
                        true
                    } else {
                        self.proc_adds += 1;
                        self.map_process(bus, &d)
                    }
                }
                mesapi::op::REMOVE_QUEUE if matches!(self.hostile, MesHostile::UnmapHangs | MesHostile::RemoveHangs) => {
                    self.sched_hung = true;
                    break;
                }
                mesapi::op::REMOVE_QUEUE => {
                    let db = d[at::REMOVE_DOORBELL];
                    let held = self.legacy.len();
                    if d[at::REMOVE_FLAGS] & mesapi::REMOVE_UNMAP_LEGACY_QUEUE != 0 {
                        let qtype = self.legacy.iter().find(|q| q.doorbell == db && q.vmid == 0).map(|q| q.qtype);
                        self.legacy.retain(|q| q.doorbell != db || q.vmid != 0);
                        if !matches!(self.hostile, MesHostile::UnmapLeavesActive | MesHostile::HqdStuck) {
                            match qtype {
                                Some(mesapi::qtype::GFX) => self.set_bank(Self::gfx_sel(), M::CpGfxHqdActive, 0),
                                Some(mesapi::qtype::COMPUTE) => self.set_bank(Self::compute_sel(), M::CpHqdActive, 0),
                                _ => {}
                            }
                        }
                    } else {
                        self.legacy.retain(|q| q.doorbell != db || q.vmid == 0);
                    }
                    self.unknown_removes += u32::from(self.legacy.len() == held);
                    true
                }
                _ => false,
            };
            if answered
                && let Some(sa) = mesapi::status_at(op)
                && !(self.hostile == MesHostile::NoApiStatus && op != mesapi::op::QUERY_SCHEDULER_STATUS)
            {
                let addr = mesapi::get64(&d, sa);
                let val = mesapi::get64(&d, sa + 2);
                let _ = bus.write(0, addr, &val.to_le_bytes());
            }
            s.rptr += mesapi::FRAME_DW as u64;
        }
        if let Some(x) = self.sched.as_mut() {
            x.rptr = s.rptr;
        }
    }

    fn map_legacy(&mut self, bus: &mut dyn GpuBus, d: &[u32]) {
        let qtype = d[at::ADD_QUEUE_TYPE];
        let db = d[at::ADD_DOORBELL];
        let m = Self::rd_mqd(bus, mesapi::get64(d, at::ADD_MQD));
        let (base, ring_dw, rptr_wb) = if qtype == mesapi::qtype::GFX {
            let base = (u64::from(m[g::HQD_BASE_HI]) << 32 | u64::from(m[g::HQD_BASE])) << 8;
            let bufsz = m[g::HQD_CNTL] & f::GFX_RB_BUFSZ_MASK;
            (base, 1u64 << (bufsz + 1).min(30), u64::from(m[g::RPTR_ADDR_HI]) << 32 | u64::from(m[g::RPTR_ADDR]))
        } else {
            let base = (u64::from(m[c::PQ_BASE_HI]) << 32 | u64::from(m[c::PQ_BASE_LO])) << 8;
            let qs = m[c::PQ_CONTROL] & f::PQ_QUEUE_SIZE_MASK;
            (base, 1u64 << (qs + 1).min(30), u64::from(m[c::RPTR_REPORT_HI]) << 32 | u64::from(m[c::RPTR_REPORT_LO]))
        };
        let limits = Limits { max_dw: 1 << 16, max_bytes: 1 << 24, max_ib_depth: 2 };
        let again = self.maps.contains(&db);
        self.maps.push(db);
        let mut engine = if again && self.hostile == MesHostile::RemapDead { None } else { Engine::new(Isa::Pm4, base, ring_dw, 0, limits).ok() };
        if qtype != mesapi::qtype::GFX {
            engine.iter_mut().for_each(Engine::refuse_non_canonical_ibs);
        }
        self.legacy.retain(|q| q.doorbell != db);
        self.legacy.push(Legacy { qtype, doorbell: db, rptr_wb, wptr: 0, engine, vmid: 0, page_table: 0 });
        if qtype == mesapi::qtype::COMPUTE {
            self.set_bank(Self::compute_sel(), M::CpHqdActive, 1);
        } else if qtype == mesapi::qtype::GFX {
            self.set_bank(Self::gfx_sel(), M::CpGfxHqdActive, 1);
        }
    }

    /// GX-7m: ADD_QUEUE of a compute process queue (see the module text). `false`: not answered (no VMID free in
    /// the mask, no GFXHUB addresses, not a compute queue).
    fn map_process(&mut self, bus: &mut dyn GpuBus, d: &[u32]) -> bool {
        use crate::ipmod::gmc11::{R as H, invalidate_req};
        let Some(a) = self.vm else { return false };
        if d[at::ADD_QUEUE_TYPE] != mesapi::qtype::COMPUTE {
            return false;
        }
        let pt = mesapi::get64(d, at::ADD_PT_BASE);
        let mask = self.hw_res.as_ref().map_or(0, |h| h[at::HW_VMID_GFXHUB]);
        let vmid = match self.legacy.iter().find(|q| q.vmid != 0 && q.page_table == pt) {
            Some(q) => q.vmid,
            None => match (1..16u8).find(|&n| mask >> n & 1 != 0 && !self.legacy.iter().any(|q| q.vmid == n)) {
                Some(n) => n,
                None => return false,
            },
        };
        let dw = |r: H| a.hub.byte(r).map(|b| b / 4);
        let base = pt | 1;
        for (r, v) in [(H::PtBaseLo(vmid), base as u32), (H::PtBaseHi(vmid), (base >> 32) as u32)] {
            if let Some(x) = dw(r) {
                let _ = bus.reg_write(x, v);
            }
        }
        if let Some(l) = a.lut(vmid) {
            let _ = bus.reg_write(l / 4, d[at::ADD_PROCESS_ID]);
        }
        if let Some(x) = dw(H::Req(FAKE_MES_ENGINE)) {
            let _ = bus.reg_write(x, invalidate_req(vmid, 0));
        }
        let m = Self::rd_mqd(bus, mesapi::get64(d, at::ADD_MQD));
        let ring = (u64::from(m[c::PQ_BASE_HI]) << 32 | u64::from(m[c::PQ_BASE_LO])) << 8;
        let ring_dw = 1u64 << ((m[c::PQ_CONTROL] & f::PQ_QUEUE_SIZE_MASK) + 1).min(30);
        let rptr_wb = u64::from(m[c::RPTR_REPORT_HI]) << 32 | u64::from(m[c::RPTR_REPORT_LO]);
        let limits = Limits { max_dw: 1 << 16, max_bytes: 1 << 24, max_ib_depth: 2 };
        let mut engine = Engine::new(Isa::Pm4, ring, ring_dw, 0, limits).ok();
        engine.iter_mut().for_each(Engine::refuse_non_canonical_ibs);
        let db = d[at::ADD_DOORBELL];
        self.legacy.retain(|q| q.doorbell != db);
        self.legacy.push(Legacy { qtype: mesapi::qtype::COMPUTE, doorbell: db, rptr_wb, wptr: 0, engine, vmid, page_table: pt });
        true
    }

    /// GX-7m: the process queues MES has mapped: (doorbell, VMID).
    pub fn processes(&self) -> Vec<(u32, u8)> {
        self.legacy.iter().filter(|q| q.vmid != 0).map(|q| (q.doorbell, q.vmid)).collect()
    }

    /// The queues MES has mapped: (queue type, doorbell).
    pub fn mapped(&self) -> Vec<(u32, u32)> {
        self.legacy.iter().filter(|q| q.vmid == 0).map(|q| (q.qtype, q.doorbell)).collect()
    }

    fn run_legacy(&mut self, bus: &mut dyn GpuBus, gfx_runs: bool, mec_runs: bool) {
        if matches!(self.hostile, MesHostile::QueuesDead | MesHostile::UnmapHangs) {
            return;
        }
        let mut sua = false;
        for q in &mut self.legacy {
            let runs = if q.qtype == mesapi::qtype::GFX { gfx_runs } else { mec_runs };
            let Some(e) = q.engine.as_mut().filter(|_| runs) else { continue };
            let mut last = None;
            let mut bus = InVmid { bus: &mut *bus, vmid: q.vmid };
            for _ in 0..64 {
                let rep = e.run(&mut bus, q.wptr);
                let more = rep.stop == Stop::Budget;
                last = Some(rep);
                if !more {
                    break;
                }
            }
            if let Some(rep) = last {
                if q.qtype == mesapi::qtype::COMPUTE && matches!(rep.stop, Stop::Hang(amdgpu_fmt::sim::Hang::Fault(_))) {
                    self.cpc_error |= f::CPC_ERROR_HALT;
                }
                sua |= matches!(rep.stop, Stop::Hang(amdgpu_fmt::sim::Hang::NotCanonicalIb { .. }));
                self.stops.push(rep.stop);
                self.events.extend(rep.events.into_iter().filter(|e| !matches!(e, Event::RegWrite { .. })));
            }
            let _ = bus.write(0, q.rptr_wb & !3, &(e.rptr() as u32).to_le_bytes());
        }
        if sua {
            let v = self.compute_hqd(M::CpHqdError) | HQD_ERROR_SUA;
            self.set_bank(Self::compute_sel(), M::CpHqdError, v);
        }
    }
}
