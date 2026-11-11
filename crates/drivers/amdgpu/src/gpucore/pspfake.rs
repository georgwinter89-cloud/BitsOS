//! GX-7a: a fake PSP 13.0.4 for the bring-up in this wave (host tests and the QEMU ktests; nothing touches the
//! real GPU). It models what linux-6.6 amdgpu_psp.c and psp_v13_0_4.c expect from the device: the mailbox
//! (C2PMSG_64 response bit and status, ring address/size in 69/70/71, the KM write pointer in 67, the sOS sign
//! of life in 81), the KM ring read from GPU memory frame by frame, command buffers with their responses and
//! fence write-back, LOAD_TOC answering a TMR size, SETUP_TMR, LOAD_IP_FW (copies are checked by a hash), RLC
//! autoload. Answers come after `latency_us` of the fake's own clock, which only `delay_us` advances, so a
//! test of a 100 ms timeout takes no real time. `Hostile` makes it misbehave in one way each.
//!
//! GX-7c: the fake grew into the whole chip the bring-up meets. The PSP side is memory-aware (the TMR it set up
//! is its own: CPU writes into it are refused and counted; buffers in reserved ranges or a TMR over them are
//! refused; LOAD_IP_FW is checked against Linux's order with AUTOLOAD_RLC right after RLC_G; a ring or TMR left
//! from an earlier kernel answers a non-zero status; C2PMSG_64 carries the real chip's extra bits). Beside it the
//! SMU fake (`smufake`: both PMFW mailboxes, FIRMWARE_FLAGS through the SMN window) and the GC fake (`gcfake`:
//! GFXOFF, IMU, RLC autoload), the scanout watcher's record (`display_writes`, `scanout_writes`) and, in
//! `fakechip`, the 780M discovery table the fake is built from and the start states (cold, after Linux, ...).
//!
//! Also here: synthetic firmware files for the 780M's twelve needs (headers like linux-firmware's, small
//! payloads), so tests and the ktest run without the real files (never in git).

use crate::bo::BoManager;
use crate::gpucore::disc::Discovery;
use crate::gpucore::gcfake::GcFake;
use crate::gpucore::smufake::SmuFake;
use crate::gpucore::watch::DisplayGuard;
use crate::gpucore::fwload::{self, FwSource, LoadError};
use crate::gpucore::psp::{self, Carve, Hw, Psp, PspError, Report, Timeouts, cmd};
use crate::gpucore::ucode::{FwSet, ListOpts, Ucode, UcodeId, Why, crc32};
use crate::ipmod::psp13::{Addrs, Mailbox13};
use crate::ipmod::{self, Choice, Plan, Role, v};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// MP0 segment 1 base of the 780M in dwords (Linux's ip_discovery sysfs view, 27.10.).
pub const MP0_SEG1: u32 = 0x16000;
/// The carve-out as the GPU sees it on the Mini-PC (Linux: VRAM at 0x8000000000) and its size on 27.10. (512 MiB;
/// the BIOS setting decides it, 8 GB as Georg usually sets it: tests set `fb_size` to other sizes, nothing else
/// in the fake depends on it).
pub const FB_MC: u64 = 0x80_0000_0000;
pub const FB_SIZE: u64 = 512 << 20;
/// The CPU physical base the fake expects in SETUP_TMR: MMMC_VM_FB_OFFSET 0x840 << 24, as the Mini-PC read it on
/// 27.10. (regs-load.csv; DCN_VM_FB_OFFSET the same). GX-7a used the spec's 0x66 (gpu.md section 2.1), wrong there.
pub const FB_PHYS: u64 = 0x8_4000_0000;
/// What LOAD_TOC answered on the Mini-PC: Linux reserved 0x4000000 for the PSP TMR (27.10.).
pub const TOC_TMR: u32 = 0x400_0000;
/// The fake's sOS sign of life.
pub const SOS_SIGN: u32 = 0x0000_00b1;

/// DESTROY_TMR's status when no TMR is left (the Mini-PC, 28.10.).
pub const NO_TMR: u32 = 0xFFFF_0007;
/// Fake status codes (the real PSP's codes are not documented; only zero/non-zero matters to Linux).
pub const ST_BAD_PARAM: u32 = 0x1;
pub const ST_NO_TMR: u32 = 0x3;
pub const ST_BAD_FW: u32 = 0x4;
pub const ST_NO_RLC: u32 = 0x5;
/// INIT_GPCOM_RING while a ring is still live, SETUP_TMR while another TMR is set (assumptions: the real PSP's
/// answer is unknown; the scout's "C2PMSG_64 never ready / non-zero status after create" covers both).
pub const ST_RING_EXISTS: u32 = 0x6;
pub const ST_TMR_EXISTS: u32 = 0x7;
/// C2PMSG_64 as the real 780M reads it before any ring command (the capture: 0x80c00000) and the bits it keeps
/// beside the command it answered (after create it read 0x80c20000: bit 31, 0xc0 << 16 and the KM type).
pub const CTRL_COLD: u32 = 0x80c0_0000;
const CTRL_STICKY: u32 = 0x00c0_0000;

/// The order Linux sends the 13.0.4 microcodes in (`amdgpu_ucode.h` enum order, amdgpu_psp.c:2560), including
/// those BitsOS skips (MES KIQ, RLC_V and restore-list CNTL when empty, DMCUB).
pub const LINUX_ORDER: &[UcodeId] = &[
    UcodeId::SdmaTh0,
    UcodeId::SdmaTh1,
    UcodeId::CpPfp,
    UcodeId::CpMe,
    UcodeId::CpMec1,
    UcodeId::CpMes,
    UcodeId::CpMesData,
    UcodeId::CpMes1,
    UcodeId::CpMes1Data,
    UcodeId::ImuI,
    UcodeId::ImuD,
    UcodeId::RlcRestoreListCntl,
    UcodeId::RlcRestoreListGpmMem,
    UcodeId::RlcRestoreListSrmMem,
    UcodeId::RlcIram,
    UcodeId::RlcDram,
    UcodeId::RlcP,
    UcodeId::RlcV,
    UcodeId::RlcG,
    UcodeId::Vcn,
    UcodeId::Dmcub,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hostile {
    None,
    /// Never answers a write (C2PMSG_64 keeps bit 31 clear, no frame runs, no fence).
    Silent,
    /// C2PMSG_64 never shows bit 31: the sOS is not ready for a ring.
    NotReady,
    /// C2PMSG_81 reads 0.
    SosDead,
    /// Every register reads all ones (device gone).
    AllOnes,
    /// Ring create answers this status.
    RingStatus(u16),
    /// Commands with this ID answer this status.
    CmdStatus { id: u32, status: u32 },
    /// Commands run, no fence is written.
    NoFence,
    /// The fence is written with a wrong value.
    WrongFence,
    /// C2PMSG_67 reads this value once the driver wrote it (a frame went in: the next push meets it).
    Wptr(u32),
    /// GX-7d: C2PMSG_67 reads this value until the driver writes it (it survives DESTROY_RINGS and create).
    WptrKept(u32),
    /// Frames are taken but run only after `release`.
    Hold,
    /// Writes to this register are refused.
    RefuseWrite(u32),
    /// LOAD_TOC answers this TMR size.
    TocTmr(u32),
    /// GPU memory accesses fail.
    MemFail,
    /// GX-7d fixer: DESTROY_RINGS is never answered (C2PMSG_64 keeps bit 31 clear; the ring stays).
    StopSilent,
}

/// One LOAD_IP_FW the fake received.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FwSeen {
    pub fw_type: u32,
    pub size: u32,
    pub mc: u64,
    /// FNV-1a over the bytes it read at `mc`.
    pub sum: u64,
}

pub struct FakeGpu {
    pub a: Addrs,
    pub hostile: Hostile,
    /// GX-9a: a job whose waves in this VMID hold it: a kill of them runs this (the test frees what the job waits for).
    pub sq_release: Option<(u8, alloc::sync::Arc<dyn Fn() + Send + Sync>)>,
    /// GX-8b: this many doorbell writes do not land (the BAR's mapping gone); the caller is told.
    pub refuse_doorbells: u32,
    /// GX-9b: runs once, at the next look at the clock (a CPU coming in between two steps of the code under test).
    pub on_now: Option<alloc::boxed::Box<dyn FnMut() + Send>>,
    /// GX-9b: messages taken when HDP was last flushed.
    pub hdp_flushed_at: Option<usize>,
    pub now: u64,
    pub latency_us: u64,
    pub fb_mc: u64,
    pub fb_size: u64,
    pub fb_phys: u64,
    regs: BTreeMap<u32, u32>,
    mem: BTreeMap<u64, Box<[u8]>>,
    pub ring: Option<(u64, u32)>,
    rptr: u32,
    wptr: u32,
    /// Writes to C2PMSG_67 so far (`Hostile::Wptr`/`WptrKept`).
    wptr_writes: u32,
    ctrl_due: Option<(u64, u32)>,
    frames_due: Option<u64>,
    toc_tmr: Option<u32>,
    pub tmr: Option<(u64, u32)>,
    tmr_used: u64,
    pub loaded: Vec<FwSeen>,
    /// Command IDs in the order they ran.
    pub cmds: Vec<u32>,
    /// Every register write, in order.
    pub writes: Vec<(u32, u32)>,
    pub autoload: bool,
    pub frames: u32,
    pub smu: SmuFake,
    pub gc: GcFake,
    pub guard: DisplayGuard,
    /// Every register write the scanout watcher flags (DCN, the display's MMHUB fields, its PMFW mailbox).
    pub display_writes: Vec<(u32, u32)>,
    /// GX-7m3: writes starting at this MC address are refused, after the first `.1` of them.
    pub refuse_mem: Option<(u64, u32)>,
    /// Memory writes into what the display reads (`guard.protect`).
    pub scanout_writes: Vec<(u64, usize)>,
    /// CPU writes into the TMR after SETUP_TMR (refused: the PSP owns it).
    pub tmr_writes: Vec<(u64, usize)>,
    /// MC ranges [lo, hi) no PSP buffer or TMR may cover (the GOP surface, DMCUB windows, the discovery blob).
    pub reserved: Vec<(u64, u64)>,
    /// LOAD_IP_FW / AUTOLOAD_RLC out of Linux's order: (previous fw type, this one; 0x21 = AUTOLOAD_RLC).
    pub order_errors: Vec<(u32, u32)>,
    /// SMN addresses read through the index/data window that the fake does not have.
    pub smn_unknown: Vec<u32>,
    /// NBIO's PCIE_INDEX2/PCIE_DATA2 (nbio_v7_7.c:202-210) as BAR5 bytes, and the index written.
    pub smn_window: (u32, u32),
    smn_index: u32,
    /// GX-7h: GFXHUB and MMHUB (`hubfake`): flush acks, the semaphore, translation, faults.
    pub hubs: super::hubfake::HubFake,
    /// GX-7i: SDMA 0 (`sdmafake`): its queue run by GR-2's interpreter, system memory, NBIO's RSMU window.
    pub sdma: super::sdmafake::SdmaFake,
    /// GX-7j: the GFX command processor (`gfxfake`): ring 0 run by GR-2's PM4 interpreter.
    pub gfx: super::gfxfake::GfxFake,
    /// GX-7k: MES (`mesfake`): both pipes, the KIQ, the scheduler's API, the queues it maps.
    pub mes: super::mesfake::MesFake,
    /// GX-7n: the IH ring (`ihfake`).
    pub ih: super::ihfake::IhFake,
}

const PAGE: u64 = 4096;

pub fn fnv(b: &[u8]) -> u64 {
    b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ u64::from(x)).wrapping_mul(0x0100_0000_01b3))
}

impl FakeGpu {
    /// A 780M-shaped fake (`fakechip::discovery_780m`), cold (`fakechip`'s image): sOS alive, ring ready.
    pub fn new(hostile: Hostile) -> FakeGpu {
        FakeGpu::with(&crate::gpucore::fakechip::discovery_780m(), hostile)
    }

    /// A fake with the register layout of `d` (MP0/MP1/GC/SMUIO/NBIO/MMHUB/DMU bases), cold, 512 MiB.
    pub fn with(d: &Discovery, hostile: Hostile) -> FakeGpu {
        FakeGpu::with_carve(d, hostile, FB_SIZE)
    }

    /// The same with a carve-out of `fb_size` bytes (any size: MEMSIZE and the FB location follow it).
    pub fn with_carve(d: &Discovery, hostile: Hostile, fb_size: u64) -> FakeGpu {
        let a = Addrs::from_discovery(d).unwrap_or(Addrs { c2pmsg_35: 0, c2pmsg_64: 0, c2pmsg_67: 0, c2pmsg_69: 0, c2pmsg_70: 0, c2pmsg_71: 0, c2pmsg_81: 0 });
        let mp1 = d.reg(crate::gpucore::hwid::MP1, 0, 1, 0).unwrap_or(MP0_SEG1);
        let nbio = |dw| d.reg(crate::gpucore::hwid::NBIF, 0, 0, dw).and_then(|b: u32| b.checked_mul(4)).unwrap_or(u32::MAX);
        let mut regs = BTreeMap::new();
        regs.insert(a.c2pmsg_81, SOS_SIGN);
        regs.insert(a.c2pmsg_64, CTRL_COLD);
        regs.insert(a.c2pmsg_35, psp::FLAG_RESPONSE);
        let mut f = FakeGpu {
            a,
            hostile,
            refuse_doorbells: 0,
            sq_release: None,
            on_now: None,
            hdp_flushed_at: None,
            now: 0,
            latency_us: 50,
            fb_mc: FB_MC,
            fb_size,
            fb_phys: FB_PHYS,
            regs,
            mem: BTreeMap::new(),
            ring: None,
            rptr: 0,
            wptr: 0,
            wptr_writes: 0,
            ctrl_due: None,
            frames_due: None,
            toc_tmr: None,
            tmr: None,
            tmr_used: 0,
            loaded: Vec::new(),
            cmds: Vec::new(),
            writes: Vec::new(),
            autoload: false,
            frames: 0,
            smu: SmuFake::new(mp1),
            gc: GcFake::new(d),
            guard: DisplayGuard::from_discovery(d),
            display_writes: Vec::new(),
            refuse_mem: None,
            scanout_writes: Vec::new(),
            tmr_writes: Vec::new(),
            reserved: Vec::new(),
            order_errors: Vec::new(),
            smn_unknown: Vec::new(),
            // BIF_BX0_PCIE_INDEX2/DATA2: NBIF segment 0, dwords 0xe/0xf (nbio_7_7_0_offset.h).
            smn_window: (nbio(0xe), nbio(0xf)),
            smn_index: 0,
            hubs: super::hubfake::HubFake::new(d),
            sdma: super::sdmafake::SdmaFake::new(d),
            gfx: super::gfxfake::GfxFake::new(d),
            mes: super::mesfake::MesFake::new(d),
            ih: super::ihfake::IhFake::new(d),
        };
        f.gfx.cold_gc(&mut f.gc);
        crate::gpucore::fakechip::cold_image(&mut f, d);
        f
    }

    /// A plain register's value as the fake starts with it (the start images of `fakechip`).
    pub fn set_reg(&mut self, byte: u32, value: u32) {
        if self.sdma.owns_gc(byte) || self.sdma.owns_port(byte) {
            let _ = self.sdma.write(byte, value);
        } else if self.gfx.owns(byte) {
            let _ = self.gfx.write(byte, value);
        } else if self.mes.owns(byte) {
            self.mes.write(byte, value);
        } else if self.hubs.owns(byte).is_some() {
            self.hubs.set(byte, value);
        } else {
            self.regs.insert(byte, value);
        }
    }

    /// GX-7h: an access of the GPU's client `cid` through `hub` (VMID 0): the host-physical address it reaches,
    /// or `None` with the fault recorded in that hub's L2 protection fault registers.
    pub fn gpu_access(&mut self, hub: crate::ipmod::gmc11::Hub, mc: u64, write: bool, cid: u32) -> Option<u64> {
        let (fb_mc, fb_phys, fb_size) = (self.fb_mc, self.fb_phys, self.fb_size);
        let mem = &self.mem;
        self.hubs.translate(hub, mc, write, cid, &mut |phys| {
            let off = phys.checked_sub(fb_phys).filter(|o| o.checked_add(8).is_some_and(|e| e <= fb_size))?;
            let at = fb_mc + off;
            let mut b = [0u8; 8];
            for (i, x) in b.iter_mut().enumerate() {
                let a = at + i as u64;
                *x = mem.get(&(a / PAGE)).map_or(0, |p| p[(a % PAGE) as usize]);
            }
            Some(u64::from_le_bytes(b))
        })
    }

    /// GX-7m2: one access through GFXHUB's walk of VMID `vmid` over this fake's memory, as an engine makes it.
    pub fn vm_access(&mut self, vmid: u8, va: u64, access: super::hubfake::Access, cid: u32) -> Option<u64> {
        let (mem, mc, phys, size) = (&self.mem, self.fb_mc, self.fb_phys, self.fb_size);
        self.hubs.translate_vm(crate::ipmod::gmc11::Hub::Gfx, vmid, va, access, cid, &|p| super::sdmafake::carve8(mem, mc, phys, size, p))
    }

    /// GX-7i: SDMA 0 runs its queue on this fake's memory (GFXHUB VMID 0: system aperture, GART).
    fn sdma_kick(&mut self) {
        let hostile = self.sdma.hostile;
        let sys = self.sdma.sys.clone();
        let host = self.sdma.host.clone();
        let mut writes = core::mem::take(&mut self.sdma.gpu_writes);
        let mut bus = super::sdmafake::FakeBus {
            hubs: &mut self.hubs,
            mem: &mut self.mem,
            sys: &sys,
            host: host.as_deref(),
            fb_mc: self.fb_mc,
            fb_phys: self.fb_phys,
            fb_size: self.fb_size,
            gpu_writes: &mut writes,
            hostile,
            cid: super::sdmafake::CID_SDMA0,
        };
        self.sdma.kick(&mut bus);
        self.sdma.gpu_writes = writes;
        self.ih_collect();
    }

    /// GX-7j: the CP runs ring 0 on this fake's memory (GFXHUB VMID 0) and GC's registers.
    fn gfx_kick(&mut self) {
        let sys = self.sdma.sys.clone();
        let host = self.sdma.host.clone();
        let mut writes = core::mem::take(&mut self.gfx.gpu_writes);
        let hostile = self.gfx.hostile;
        let mem = super::sdmafake::FakeBus {
            hubs: &mut self.hubs,
            mem: &mut self.mem,
            sys: &sys,
            host: host.as_deref(),
            fb_mc: self.fb_mc,
            fb_phys: self.fb_phys,
            fb_size: self.fb_size,
            gpu_writes: &mut writes,
            hostile: super::sdmafake::SdmaHostile::None,
            cid: super::gfxfake::CID_CPG,
        };
        let mut bus = super::gfxfake::GfxBus { mem, gc: &mut self.gc, hostile };
        self.gfx.kick(&mut bus);
        self.gfx.gpu_writes = writes;
        self.ih_collect();
    }

    /// GX-7k: MES runs what a doorbell moved (its KIQ, its scheduler, a mapped queue) on this fake's memory
    /// (GFXHUB VMID 0) and GC's registers; a mapped queue runs only while its engine (ME/PFP, MEC ME1) runs.
    fn mes_kick(&mut self, what: super::mesfake::Rang, value: u64) {
        // GX-7m3: GFX queues' accesses come from CPG, compute queues' from CPC (the chip's two fault decodes).
        if what == super::mesfake::Rang::Legacy {
            self.mes_kick_as(what, value, super::gfxfake::CID_CPG, true, false);
            self.mes_kick_as(what, value, super::gfxfake::CID_CPC, false, true);
        } else {
            self.mes_kick_as(what, value, super::gfxfake::CID_CPG, true, true);
        }
    }

    fn mes_kick_as(&mut self, what: super::mesfake::Rang, value: u64, cid: u32, gfx: bool, mec: bool) {
        let sys = self.sdma.sys.clone();
        let host = self.sdma.host.clone();
        let mut writes = core::mem::take(&mut self.gfx.gpu_writes);
        let gfx_runs = gfx && self.gfx.get(crate::ipmod::gfx11::R::CpMeCntl) & crate::ipmod::gfx11::f::GFX_HALTS == 0 && !self.gfx.stuck();
        let mec_runs = mec && !self.gfx.mec_halted();
        let mem = super::sdmafake::FakeBus {
            hubs: &mut self.hubs,
            mem: &mut self.mem,
            sys: &sys,
            host: host.as_deref(),
            fb_mc: self.fb_mc,
            fb_phys: self.fb_phys,
            fb_size: self.fb_size,
            gpu_writes: &mut writes,
            hostile: super::sdmafake::SdmaHostile::None,
            cid,
        };
        let mut bus = super::gfxfake::GfxBus { mem, gc: &mut self.gc, hostile: super::gfxfake::GfxHostile::None };
        self.mes.kick(&mut bus, what, value, gfx_runs, mec_runs);
        self.gfx.gpu_writes = writes;
        self.ih_collect();
    }

    /// GX-7n: the queues MES mapped look at their memory again (the chip's WAIT_REG_MEM keeps polling; the interpreter has
    /// to be asked).
    pub fn rerun_legacy(&mut self) {
        self.mes_kick(super::mesfake::Rang::Legacy, 0);
    }

    /// GX-7n: an entry on the ring, as an engine's or a test's. False: the ring is off.
    pub fn ih_raise(&mut self, e: &crate::ipmod::ih6::Iv) -> bool {
        use crate::ipmod::ih6::{self, f};
        let Some(a) = self.ih.a else { return false };
        let cntl = self.reg(a.cntl);
        if cntl & f::RB_ENABLE == 0 {
            self.ih.refused += 1;
            return false;
        }
        let base = u64::from(self.reg(a.base)) << 8 | u64::from(self.reg(a.base_hi) & 0xff) << 40;
        let wb = u64::from(self.reg(a.wptr_lo)) | u64::from(self.reg(a.wptr_hi) & 0xffff) << 32;
        let bytes: Vec<u8> = e.encode().iter().flat_map(|w| w.to_le_bytes()).collect();
        if !self.poke(base + u64::from(self.ih.wptr), &bytes) {
            self.ih.refused += 1;
            return false;
        }
        let next = (self.ih.wptr + ih6::ENTRY_BYTES) & (ih6::RING_BYTES - 1);
        self.ih.overflow |= next == self.ih.rptr && cntl & f::WPTR_OVERFLOW_ENABLE != 0;
        self.ih.wptr = next;
        self.ih.written += 1;
        if cntl & f::WPTR_WRITEBACK_ENABLE != 0 {
            let v = next | u32::from(self.ih.overflow);
            let _ = self.poke(wb, &v.to_le_bytes());
        }
        if cntl & f::ENABLE_INTR != 0 {
            self.ih.send();
        }
        true
    }

    /// GX-7n: what the engines raised since the last look, with their enable bits on; a GFXHUB fault likewise.
    fn ih_collect(&mut self) {
        use crate::gpucore::ih::{TIME_STAMP_INT_ENABLE, TRAP_ENABLE, VM_FAULT_INTERRUPTS};
        use crate::ipmod::ih6::{CLIENT_GFX, CLIENT_GRBM_CP, SRC_CP_EOP, SRC_SDMA_TRAP, SRC_UTCL2_FAULT};
        use amdgpu_fmt::sim::{Event, IntSrc};
        let Some(a) = self.ih.a else { return };
        let ints = |evs: &[Event], seen: usize| -> Vec<IntSrc> { evs.iter().skip(seen).filter_map(|e| if let Event::Interrupt { src, .. } = e { Some(*src) } else { None }).collect() };
        let new_sdma = ints(&self.sdma.events, self.ih.seen[0]);
        let new_gfx = ints(&self.gfx.events, self.ih.seen[1]);
        let new_mes = ints(&self.mes.events, self.ih.seen[2]);
        let seen_faults = self.ih.seen[3];
        self.ih.seen = [self.sdma.events.len(), self.gfx.events.len(), self.mes.events.len(), self.hubs.faults.len()];
        if self.reg_read(a.sdma0_cntl).unwrap_or(0) & TRAP_ENABLE != 0 {
            for s in &new_sdma {
                if *s == IntSrc::Trap {
                    self.ih_raise(&super::ihfake::iv(CLIENT_GFX, SRC_SDMA_TRAP, 0, 0, 0));
                }
            }
        }
        // Which pipe a queue of MES's is on is the firmware's: any of the enabled bits lets it through.
        let ring0 = self.reg_read(a.cp_int_ring0).unwrap_or(0) & TIME_STAMP_INT_ENABLE != 0;
        let me1 = a.cp_me1_pipe.iter().all(|&p| self.reg_read(p).unwrap_or(0) & TIME_STAMP_INT_ENABLE != 0);
        for s in new_gfx.iter().chain(&new_mes) {
            if *s == IntSrc::EndOfPipe && ring0 && me1 {
                self.ih_raise(&super::ihfake::iv(CLIENT_GRBM_CP, SRC_CP_EOP, 0, 0, 0));
            }
        }
        let faults: Vec<super::hubfake::Faulted> = self.hubs.faults.iter().skip(seen_faults).copied().collect();
        for x in faults {
            if x.hub == crate::ipmod::gmc11::Hub::Gfx && self.reg_read(a.gc_ctx[usize::from(x.vmid) & 15]).unwrap_or(0) & VM_FAULT_INTERRUPTS != 0 {
                self.ih_raise(&super::ihfake::iv(CLIENT_GFX, SRC_UTCL2_FAULT, 0, x.vmid, (x.mc >> 12) as u32));
            }
        }
    }

    /// GX-7n: the view the IH thread has past the chain.
    pub fn ih_entry(&self, off: u32, out: &mut [u32; 8]) -> bool {
        let Some(a) = self.ih.a else { return false };
        let base = u64::from(self.reg(a.base)) << 8 | u64::from(self.reg(a.base_hi) & 0xff) << 40;
        let mut b = [0u8; 32];
        if !self.peek(base + u64::from(off), &mut b) {
            return false;
        }
        for (o, c) in out.iter_mut().zip(b.chunks_exact(4)) {
            *o = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        true
    }

    pub fn ih_wptr_wb(&self) -> Option<u32> {
        let a = self.ih.a?;
        let wb = u64::from(self.reg(a.wptr_lo)) | u64::from(self.reg(a.wptr_hi) & 0xffff) << 32;
        let mut b = [0u8; 4];
        self.peek(wb, &mut b).then(|| u32::from_le_bytes(b))
    }

    /// The doorbell of ih_v6_0_set_rptr, with its conditions: the aperture, IH_DOORBELL_RPTR, the range.
    fn ih_doorbell(&mut self, value: u32) {
        use crate::ipmod::ih6::{self, f};
        let Some(a) = self.ih.a else { return };
        let aper = self.sdma.addrs.and_then(|x| x.nbio).is_some_and(|n| self.regs.get(&n.aper_en).is_some_and(|v| v & 1 != 0));
        let on = self.reg(a.doorbell_rptr) & f::DOORBELL_ENABLE != 0;
        let range = self.sdma.port.get(&a.range).copied().unwrap_or(0);
        if !(aper && on && range == ih6::doorbell_range(range)) {
            return;
        }
        if self.ih.set_rptr(value) && self.reg(a.cntl) & f::RPTR_REARM != 0 && self.reg(a.cntl) & f::ENABLE_INTR != 0 {
            self.ih.send();
        }
    }

    /// A KM ring an earlier kernel left: live at `mc` with `size` bytes, write and read pointer at `wptr`.
    pub fn stale_ring(&mut self, mc: u64, size: u32, wptr: u32) {
        self.ring = Some((mc, size));
        self.rptr = wptr;
        self.wptr = wptr;
        self.regs.insert(self.a.c2pmsg_69, mc as u32);
        self.regs.insert(self.a.c2pmsg_70, (mc >> 32) as u32);
        self.regs.insert(self.a.c2pmsg_71, size);
        self.regs.insert(self.a.c2pmsg_64, CTRL_COLD | psp::CTRL_INIT_GPCOM_RING);
    }

    /// A TMR an earlier kernel set up and never destroyed.
    pub fn stale_tmr(&mut self, mc: u64, size: u32) {
        self.tmr = Some((mc, size));
        self.tmr_used = 0;
    }

    /// `Hold`: run the frames taken so far.
    pub fn release(&mut self) {
        if self.hostile == Hostile::Hold {
            self.hostile = Hostile::None;
        }
        self.frames_due = Some(self.now);
        self.tick();
    }

    pub fn reg(&self, byte: u32) -> u32 {
        if self.gfx.owns(byte) {
            return self.gfx.read(byte);
        }
        if self.sdma.owns_gc(byte) {
            return self.sdma.addrs.and_then(|a| crate::ipmod::sdma6::R::ALL.into_iter().find(|&r| a.byte(r) == Some(byte))).map_or(0, |r| self.sdma.get(r));
        }
        if let Some(v) = self.hubs.lut_read(byte) {
            return v;
        }
        if self.hubs.owns(byte).is_some() {
            let mut h = 0;
            if let Some((hub, r)) = [self.hubs.gfx, self.hubs.mm].into_iter().flatten().find_map(|a| a.reg_at(byte).map(|r| (a.hub, r))) {
                h = self.hubs.get(hub, r);
            }
            return h;
        }
        self.regs.get(&byte).copied().unwrap_or(0)
    }

    fn in_fb(&self, mc: u64, len: usize) -> bool {
        mc >= self.fb_mc && mc.checked_add(len as u64).is_some_and(|e| e <= self.fb_mc + self.fb_size)
    }

    fn peek(&self, mc: u64, buf: &mut [u8]) -> bool {
        if !self.in_fb(mc, buf.len()) {
            return false;
        }
        for (i, b) in buf.iter_mut().enumerate() {
            let at = mc + i as u64;
            *b = self.mem.get(&(at / PAGE)).map_or(0, |p| p[(at % PAGE) as usize]);
        }
        true
    }

    /// GX-8a: `len` bytes from the page-aligned `mc` to zero, as the backend wipes a VRAM object, seen by the same
    /// records as `mem_write`: a wipe of many GiB costs no fake memory.
    pub fn wipe(&mut self, mc: u64, len: u64) -> bool {
        if !self.in_fb(mc, len as usize) || mc % PAGE != 0 || len % PAGE != 0 {
            return false;
        }
        if self.guard.mem_class(mc, len as usize).is_some() {
            self.scanout_writes.push((mc, len as usize));
        }
        if let Some((tmc, size)) = self.tmr
            && mc < tmc + u64::from(size)
            && mc.saturating_add(len) > tmc
        {
            self.tmr_writes.push((mc, len as usize));
            return false;
        }
        let pages: alloc::vec::Vec<u64> = self.mem.range(mc / PAGE..(mc + len) / PAGE).map(|(k, _)| *k).collect();
        for p in pages {
            self.mem.remove(&p);
        }
        true
    }

    fn poke(&mut self, mc: u64, data: &[u8]) -> bool {
        if !self.in_fb(mc, data.len()) {
            return false;
        }
        let mut i = 0usize;
        while i < data.len() {
            let at = mc + i as u64;
            let off = (at % PAGE) as usize;
            let n = (PAGE as usize - off).min(data.len() - i);
            let page = self.mem.entry(at / PAGE).or_insert_with(|| vec![0u8; PAGE as usize].into_boxed_slice());
            page[off..off + n].copy_from_slice(&data[i..i + n]);
            i += n;
        }
        true
    }

    fn word(&self, mc: u64) -> u32 {
        let mut b = [0u8; 4];
        self.peek(mc, &mut b);
        u32::from_le_bytes(b)
    }

    fn reserved_hit(&self, mc: u64, len: u64) -> bool {
        let end = mc.saturating_add(len);
        self.reserved.iter().any(|&(lo, hi)| mc < hi && end > lo)
    }

    /// Linux's order: each image after the one before it in `LINUX_ORDER` (skipping is fine).
    fn check_order(&mut self, ty: u32) {
        let pos = |t: u32| LINUX_ORDER.iter().position(|u| u.fw_type() == t);
        let prev = self.loaded.last().map(|f| f.fw_type);
        match (prev.map(pos), pos(ty)) {
            (_, None) => self.order_errors.push((prev.unwrap_or(0), ty)),
            (Some(Some(p)), Some(q)) if q <= p => self.order_errors.push((prev.unwrap_or(0), ty)),
            _ => {}
        }
    }

    fn tick(&mut self) {
        self.smu.tick(self.now);
        // GX-9b: PMFW writes its tables itself: no CPU store for the watchers to judge.
        for (mc, bytes) in core::mem::take(&mut self.smu.table_writes) {
            self.poke(mc, &bytes);
        }
        for ev in core::mem::take(&mut self.smu.events) {
            self.gc.smu_event(ev, self.now);
        }
        self.gc.tick(self.now);
        if let Some((t, v)) = self.ctrl_due
            && t <= self.now
        {
            self.ctrl_due = None;
            self.ctrl(v);
        }
        if let Some(t) = self.frames_due
            && t <= self.now
            && !matches!(self.hostile, Hostile::Hold | Hostile::Silent)
        {
            self.frames_due = None;
            self.run_frames();
        }
    }

    fn ctrl(&mut self, v: u32) {
        let a = self.a;
        let status = match v {
            psp::CTRL_INIT_GPCOM_RING => {
                let mc = u64::from(self.reg(a.c2pmsg_69)) | (u64::from(self.reg(a.c2pmsg_70)) << 32);
                let size = self.reg(a.c2pmsg_71);
                let ok = size != 0 && size % psp::FRAME_BYTES == 0 && mc % PAGE == 0 && self.in_fb(mc, size as usize) && !self.reserved_hit(mc, u64::from(size));
                match self.hostile {
                    Hostile::RingStatus(s) => u32::from(s),
                    _ if self.ring.is_some() => ST_RING_EXISTS,
                    _ if !ok => ST_BAD_PARAM,
                    _ => {
                        self.ring = Some((mc, size));
                        self.rptr = 0;
                        self.wptr = 0;
                        0
                    }
                }
            }
            psp::CTRL_DESTROY_RINGS => {
                self.ring = None;
                0
            }
            _ => ST_BAD_PARAM,
        };
        self.regs.insert(a.c2pmsg_64, psp::FLAG_RESPONSE | CTRL_STICKY | (v & 0x7FFF_0000) | (status & psp::STATUS_MASK));
    }

    fn run_frames(&mut self) {
        let Some((rmc, size)) = self.ring else { return };
        let ring_dw = size / 4;
        let frame_dw = psp::FRAME_BYTES / 4;
        let target = self.wptr;
        if target % frame_dw != 0 || target >= ring_dw {
            return; // the real PSP's reaction is unknown; the fake runs nothing
        }
        for _ in 0..size / psp::FRAME_BYTES {
            if self.rptr == target {
                break;
            }
            let mut f = [0u8; psp::FRAME_BYTES as usize];
            self.peek(rmc + u64::from(self.rptr) * 4, &mut f);
            let w = |i: usize| u32::from_le_bytes([f[i * 4], f[i * 4 + 1], f[i * 4 + 2], f[i * 4 + 3]]);
            let cmd_mc = u64::from(w(0)) | (u64::from(w(1)) << 32);
            let fence_mc = u64::from(w(3)) | (u64::from(w(4)) << 32);
            let value = w(5);
            let (status, fw_addr, tmr_size) = self.command(cmd_mc);
            let mut r = [0u8; 20];
            r[0..4].copy_from_slice(&status.to_le_bytes());
            r[8..12].copy_from_slice(&(fw_addr as u32).to_le_bytes());
            r[12..16].copy_from_slice(&((fw_addr >> 32) as u32).to_le_bytes());
            r[16..20].copy_from_slice(&tmr_size.to_le_bytes());
            self.poke(cmd_mc + psp::RESP_AT as u64, &r);
            match self.hostile {
                Hostile::NoFence => {}
                Hostile::WrongFence => {
                    self.poke(fence_mc, &(value ^ 0x55).to_le_bytes());
                }
                _ => {
                    self.poke(fence_mc, &value.to_le_bytes());
                }
            }
            self.rptr = (self.rptr + frame_dw) % ring_dw;
            self.frames += 1;
        }
    }

    /// Runs one command buffer: (status, fw_addr, tmr_size).
    fn command(&mut self, mc: u64) -> (u32, u64, u32) {
        let id = self.word(mc + psp::CMD_ID_AT as u64);
        let w = |s: &Self, i: u64| s.word(mc + psp::CMD_AT as u64 + i * 4);
        let q = |s: &Self, i: u64| u64::from(w(s, i)) | (u64::from(w(s, i + 1)) << 32);
        self.cmds.push(id);
        // GX-7d: a command the PSP refuses has no effect (no TMR set, no image taken).
        if let Hostile::CmdStatus { id: i, status: s } = self.hostile
            && i == id
        {
            return (s, 0, 0);
        }
        let (mut status, mut fw_addr, mut tmr_size) = (0u32, 0u64, 0u32);
        match id {
            cmd::LOAD_TOC => {
                if w(self, 2) == 0 {
                    status = ST_BAD_PARAM;
                } else {
                    tmr_size = if let Hostile::TocTmr(t) = self.hostile { t } else { TOC_TMR };
                    self.toc_tmr = Some(tmr_size);
                }
            }
            cmd::SETUP_TMR => {
                let (tmc, size, flags, pa) = (q(self, 0), w(self, 2), w(self, 3), q(self, 4));
                let ok = tmc % psp::TMR_ALIGN == 0
                    && size != 0
                    && self.toc_tmr.is_none_or(|t| size >= t)
                    && flags & psp::TMR_VIRT_PHY != 0
                    && self.in_fb(tmc, size as usize)
                    && !self.reserved_hit(tmc, u64::from(size))
                    && pa == self.fb_phys + (tmc - self.fb_mc);
                if self.tmr.is_some_and(|t| t != (tmc, size)) {
                    status = ST_TMR_EXISTS;
                } else if ok {
                    self.tmr = Some((tmc, size));
                    self.tmr_used = 0;
                } else {
                    status = ST_BAD_PARAM;
                }
            }
            cmd::LOAD_IP_FW => {
                let (fmc, size, ty) = (q(self, 0), w(self, 2), w(self, 3));
                match self.tmr {
                    None => status = ST_NO_TMR,
                    Some((tmc, tsize)) if fmc % PAGE == 0 && size != 0 && ty != 0 && ty < 98 => {
                        let mut b = vec![0u8; size as usize];
                        let in_tmr = fmc < tmc + u64::from(tsize) && fmc + u64::from(size) > tmc;
                        if !in_tmr && self.peek(fmc, &mut b) {
                            self.check_order(ty);
                            self.loaded.push(FwSeen { fw_type: ty, size, mc: fmc, sum: fnv(&b) });
                            fw_addr = tmc + self.tmr_used;
                            self.tmr_used += u64::from(size).div_ceil(PAGE) * PAGE;
                        } else {
                            status = ST_BAD_FW;
                        }
                    }
                    Some(_) => status = ST_BAD_FW,
                }
            }
            cmd::AUTOLOAD_RLC => {
                if self.loaded.last().is_none_or(|f| f.fw_type != UcodeId::RlcG.fw_type()) {
                    self.order_errors.push((self.loaded.last().map_or(0, |f| f.fw_type), cmd::AUTOLOAD_RLC));
                }
                if self.loaded.iter().any(|f| f.fw_type == 8) {
                    self.autoload = true;
                    let types: Vec<u32> = self.loaded.iter().map(|f| f.fw_type).collect();
                    // GX-7j3: before the RLC restarts, PMFW meets the load under its running IMU (29.10. model).
                    self.smu.warm_load(self.gc.imu_started);
                    self.gc.autoload_rlc(&types, self.now);
                } else {
                    status = ST_NO_RLC;
                }
            }
            // The Mini-PC 28.10.: DESTROY_TMR with no TMR left answers 0xffff0007.
            cmd::DESTROY_TMR => {
                if self.tmr.take().is_none() {
                    status = NO_TMR;
                }
            }
            _ => status = psp::PSP_ERR_UNKNOWN_COMMAND,
        }
        (status, fw_addr, tmr_size)
    }
}

impl Hw for FakeGpu {
    fn reg_read(&mut self, byte: u32) -> Option<u32> {
        self.tick();
        self.gfx.now = self.now;
        let a = self.a;
        Some(match self.hostile {
            Hostile::AllOnes => u32::MAX,
            Hostile::SosDead if byte == a.c2pmsg_81 => 0,
            Hostile::NotReady if byte == a.c2pmsg_64 => 0,
            Hostile::Wptr(v) if byte == a.c2pmsg_67 && self.wptr_writes > 0 => v,
            Hostile::WptrKept(v) if byte == a.c2pmsg_67 && self.wptr_writes == 0 => v,
            _ if byte == a.c2pmsg_67 => self.wptr,
            _ if self.ih.owns_ptr(byte) => self.ih.read_ptr(byte),
            // SDMA0 lives in GC: gated with it.
            _ if self.sdma.owns_gc(byte) && self.gc.power != super::gcfake::Power::On => self.gc.read(byte).unwrap_or(u32::MAX),
            _ if self.sdma.owns_gc(byte) || self.sdma.owns_port(byte) => self.sdma.read(byte),
            // GX-7j: the CP's registers live in GC: gated with it.
            _ if self.gfx.owns(byte) && self.gc.power != super::gcfake::Power::On => self.gc.read(byte).unwrap_or(u32::MAX),
            _ if self.gfx.owns(byte) => self.gfx.read(byte),
            // GX-7k: MES lives in GC: gated with it.
            _ if self.mes.owns(byte) && self.gc.power != super::gcfake::Power::On => self.gc.read(byte).unwrap_or(u32::MAX),
            _ if self.mes.owns(byte) => self.mes.read(byte),
            // GX-7j2: CP_STAT of a booted GC is the CP's (the GC fake's while it boots or with its CpBusy).
            _ if byte == self.gc.cp_stat && self.gc.power == super::gcfake::Power::On => match self.gc.read(byte) {
                Some(0) => self.gfx.cp_stat(),
                v => v.unwrap_or(u32::MAX),
            },
            // GX-7m: the IH's VMID-to-PASID table (OSSSYS, not gated with GC).
            _ if self.hubs.lut_owns(byte) => self.hubs.lut_read(byte).unwrap_or(0),
            _ if self.hubs.owns(byte).is_some() => {
                if self.hubs.owns(byte) == Some(crate::ipmod::gmc11::Hub::Gfx) && self.gc.power != super::gcfake::Power::On {
                    self.gc.read(byte).unwrap_or(u32::MAX)
                } else {
                    self.hubs.read(byte).unwrap_or(0)
                }
            }
            _ if byte == self.smn_window.1 => match self.smu.smn_read(self.smn_index) {
                Some(v) => v,
                None => {
                    self.smn_unknown.push(self.smn_index);
                    0
                }
            },
            _ => match self.smu.read(byte).or_else(|| self.gc.read(byte)) {
                Some(v) => v,
                None => self.reg(byte),
            },
        })
    }

    fn reg_write(&mut self, byte: u32, value: u32) -> bool {
        if self.hostile == Hostile::RefuseWrite(byte) {
            return false;
        }
        self.tick();
        self.gfx.now = self.now;
        self.writes.push((byte, value));
        if self.guard.class(byte).is_some_and(|c| c.is_display()) {
            self.display_writes.push((byte, value));
        }
        // Through the SMN window: recorded by its SMN target (an address below BAR5's end is the BAR5 byte).
        if byte == self.smn_window.1 && self.guard.smn_class(self.smn_index).is_some() {
            self.display_writes.push((self.smn_index, value));
        }
        let a = self.a;
        if byte == self.smn_window.0 {
            self.smn_index = value;
        } else if self.ih.is_cntl(byte) {
            use crate::ipmod::ih6::f;
            if value & f::WPTR_OVERFLOW_CLEAR != 0 {
                self.ih.overflow = false;
            }
            if value & f::RB_ENABLE == 0 {
                self.ih.off();
            }
            self.regs.insert(byte, value & !f::WPTR_OVERFLOW_CLEAR);
        } else if self.ih.owns_ptr(byte) {
            if self.ih.a.is_some_and(|x| byte == x.wptr) {
                self.ih.wptr = value & (crate::ipmod::ih6::RING_BYTES - 1) & !1;
                self.ih.overflow = false;
            } else if self.ih.set_rptr(value) {
                let cntl = self.ih.a.map_or(0, |x| self.reg(x.cntl));
                if cntl & crate::ipmod::ih6::f::RPTR_REARM != 0 && cntl & crate::ipmod::ih6::f::ENABLE_INTR != 0 {
                    self.ih.send();
                }
            }
        } else if self.smu.write(byte, value, self.now) {
            // PMFW takes the message now: its effect on GC starts now, its answer comes later.
            for ev in core::mem::take(&mut self.smu.events) {
                self.gc.smu_event(ev, self.now);
            }
        } else if self.sdma.owns_gc(byte) && self.gc.power != super::gcfake::Power::On {
            // SDMA0 lives in GC: while GC is off its writes are lost (and recorded by the GC fake).
            self.gc.write(byte, value);
        } else if self.sdma.owns_gc(byte) || self.sdma.owns_port(byte) {
            if self.sdma.write(byte, value) {
                self.sdma_kick();
            }
        } else if self.gfx.owns(byte) && self.gc.power != super::gcfake::Power::On {
            // The CP lives in GC: while GC is off its writes are lost (and recorded by the GC fake).
            self.gc.write(byte, value);
        } else if self.gfx.owns(byte) {
            if self.gfx.write(byte, value) {
                self.gfx_kick();
            }
        } else if self.mes.owns(byte) && self.gc.power != super::gcfake::Power::On {
            self.gc.write(byte, value);
        } else if self.mes.owns(byte) {
            self.mes.write(byte, value);
            // GX-9a: the waves that held the job are gone: the CP goes on.
            if self.mes.byte_of_sq(byte) && let Some((vmid, go)) = self.sq_release.clone() && value == crate::ipmod::mes11::f::sq_kill(vmid) {
                go();
                self.rerun_legacy();
            }
        } else if self.hubs.lut_owns(byte) {
            self.hubs.lut_write(byte, value);
        } else if let Some(h) = self.hubs.owns(byte) {
            // GFXHUB lives in GC: while GC is off its writes are lost (and recorded by the GC fake).
            if h == crate::ipmod::gmc11::Hub::Gfx && self.gc.power != super::gcfake::Power::On {
                self.gc.write(byte, value);
            } else {
                self.hubs.write(byte, value);
            }
        } else if self.gc.owns(byte) {
            // GX-7k: MES follows GRBM_GFX_CNTL's selection (the GC fake keeps the register).
            if self.mes.is_grbm(byte) && self.gc.power == super::gcfake::Power::On {
                self.mes.grbm_write(value);
            }
            self.gc.write(byte, value);
        } else if byte == a.c2pmsg_64 {
            self.regs.insert(byte, value & !psp::FLAG_RESPONSE);
            if self.hostile != Hostile::Silent && !(self.hostile == Hostile::StopSilent && value == psp::CTRL_DESTROY_RINGS) {
                self.ctrl_due = Some((self.now + self.latency_us, value));
            }
        } else if byte == a.c2pmsg_67 {
            self.wptr_writes += 1;
            self.wptr = value;
            self.frames_due = Some(self.now + self.latency_us);
        } else {
            self.regs.insert(byte, value);
        }
        true
    }

    fn mem_read(&mut self, mc: u64, buf: &mut [u8]) -> bool {
        self.tick();
        self.hostile != Hostile::MemFail && self.peek(mc, buf)
    }

    fn mem_write(&mut self, mc: u64, data: &[u8]) -> bool {
        if self.guard.mem_class(mc, data.len()).is_some() {
            self.scanout_writes.push((mc, data.len()));
        }
        if let Some((tmc, size)) = self.tmr
            && mc < tmc + u64::from(size)
            && mc.saturating_add(data.len() as u64) > tmc
        {
            self.tmr_writes.push((mc, data.len()));
            return false;
        }
        if let Some((at, skip)) = &mut self.refuse_mem
            && *at == mc
        {
            if *skip == 0 {
                return false;
            }
            *skip -= 1;
        }
        self.hostile != Hostile::MemFail && self.poke(mc, data)
    }

    fn now_us(&mut self) -> u64 {
        if let Some(mut hook) = self.on_now.take() {
            hook();
        }
        self.now
    }

    fn flush_hdp(&mut self) {
        self.hdp_flushed_at = Some(self.smu.taken.len());
    }

    fn doorbell_write64(&mut self, index_dw: u32, value: u64) -> bool {
        if self.refuse_doorbells > 0 {
            self.refuse_doorbells -= 1;
            return false;
        }
        if index_dw == crate::ipmod::ih6::DOORBELL_DW {
            self.ih_doorbell(value as u32);
            return true;
        }
        let aper = self.sdma.addrs.and_then(|a| a.nbio).is_some_and(|n| self.regs.get(&n.aper_en).is_some_and(|v| v & 1 != 0));
        if self.gc.power == super::gcfake::Power::On && self.mes.has_doorbell(index_dw) {
            // GX-7k: MES's rings and the queues it mapped (GFX ring 0's doorbell too, once MES-mapped).
            if let Some(w) = self.mes.doorbell(index_dw, value, aper) {
                self.mes_kick(w, value);
            }
        } else if self.gc.power == super::gcfake::Power::On && index_dw == crate::ipmod::gfx11::DOORBELL_DW {
            if self.gfx.doorbell(index_dw, value, aper) {
                self.gfx_kick();
            }
        } else if self.gc.power == super::gcfake::Power::On && self.sdma.doorbell(index_dw, value, aper) {
            self.sdma_kick();
        }
        // The BAR takes the write either way (a doorbell nobody listens to is dropped silently, as on the chip).
        true
    }

    fn delay_us(&mut self, us: u64) {
        self.now = self.now.saturating_add(us);
        self.tick();
    }
}

// ---- synthetic firmware ----

/// The 780M's plan without a discovery table: the modules its versions choose (GX-6's test of the real table
/// checks that `ipmod::plan` gives the same).
pub fn plan_780m() -> Plan {
    let m = |role, version, module| Choice::Chosen { role, version, module };
    Plan {
        choices: vec![
            m(Role::Psp, v(13, 0, 4), &ipmod::psp13::MODULE),
            m(Role::Gfx, v(11, 0, 1), &ipmod::gfx11::MODULE),
            m(Role::Mes, v(11, 0, 1), &ipmod::mes11::MODULE),
            m(Role::Sdma, v(6, 0, 1), &ipmod::sdma6::MODULE),
            m(Role::Display, v(3, 1, 4), &ipmod::dcn31::MODULE),
            m(Role::Vcn, v(4, 0, 2), &ipmod::vcn4::MODULE),
        ],
    }
}

struct File {
    b: Vec<u8>,
}

impl File {
    /// A file with a `hsize`-byte header of version major.minor, the microcode array at 256.
    fn new(major: u16, minor: u16, hsize: u32, ucode: u32, extra: u32, seed: u8) -> File {
        let total = 256 + ucode + extra;
        let mut b = vec![0u8; total as usize];
        for (i, x) in b.iter_mut().enumerate().skip(256) {
            *x = (i as u8).wrapping_mul(7).wrapping_add(seed);
        }
        let mut f = File { b };
        f.set(0, total);
        f.set(4, hsize);
        f.b[8..10].copy_from_slice(&major.to_le_bytes());
        f.b[10..12].copy_from_slice(&minor.to_le_bytes());
        f.set(16, 0x100 + u32::from(seed));
        f.set(20, ucode);
        f.set(24, 256);
        let crc = crc32(&f.b[256..(256 + ucode) as usize]);
        f.set(28, crc);
        f
    }

    fn set(&mut self, at: usize, v: u32) -> &mut Self {
        self.b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        self
    }
}

/// Synthetic files for the 780M's needs, keyed by stick path. Layouts follow the real files (header sizes and
/// versions as linux-firmware-amdgpu 20261019 has them, parts in the same places), payloads small.
pub fn synthetic_780m() -> BTreeMap<String, Vec<u8>> {
    use fwload::stick_path;
    let mut out = BTreeMap::new();
    let mut add = |name: &str, f: File| {
        out.insert(stick_path(&alloc::format!("amdgpu/{name}.bin")), f.b);
    };
    add("gc_11_0_1_pfp", File::new(1, 0, 44, 0x2000, 0, 1));
    add("gc_11_0_1_me", File::new(1, 0, 44, 0x2000, 0, 2));
    let mut mec = File::new(1, 0, 44, 0x2400, 0, 3);
    mec.set(36, (0x2400 - 0x380) / 4).set(40, 0x380 / 4);
    add("gc_11_0_1_mec", mec);
    // RLC v2.3 (header 204): RLC_G 0x1000 at the array, then GPM, SRM, IRAM, DRAM, RLC_P; CNTL and RLC_V empty.
    let mut rlc = File::new(2, 3, 204, 0x1000, 0x1700, 4);
    let base = 256 + 0x1000;
    rlc.set(76, base).set(84, base);
    rlc.set(120, base).set(132, 0x200).set(136, base).set(148, 0x300).set(152, base + 0x200);
    rlc.set(156, 0x800).set(160, base + 0x500).set(164, 0x400).set(168, base + 0xD00);
    rlc.set(180, 0x200).set(184, base + 0x1100);
    add("gc_11_0_1_rlc", rlc);
    let mut imu = File::new(1, 0, 48, 0x1800, 0, 5);
    imu.set(32, 0x1000).set(40, 0x800).set(44, 0x1000);
    add("gc_11_0_1_imu", imu);
    for (name, seed) in [("gc_11_0_1_mes_2", 6u8), ("gc_11_0_1_mes1", 7)] {
        let mut m = File::new(1, 0, 72, 0x1800, 0, seed);
        m.set(36, 0x1000).set(40, 256).set(48, 0x800).set(52, 256 + 0x1000);
        // GX-7k: the uc start the 780M's MES headers name (the MES fake starts only at this program counter).
        m.set(56, super::mesfake::UC_START as u32).set(60, (super::mesfake::UC_START >> 32) as u32);
        add(name, m);
    }
    let mut sdma = File::new(2, 0, 64, 0x800, 0x700, 8);
    sdma.set(36, 0x800).set(48, 256 + 0x800).set(52, 0x700);
    add("sdma_6_0_1", sdma);
    add("vcn_4_0_2", File::new(1, 0, 32, 0x3000, 0, 9));
    let mut dmcub = File::new(1, 0, 40, 0x1800, 0, 10);
    dmcub.set(32, 0x1800);
    add("dcn_3_1_4_dmcub", dmcub);
    add("psp_13_0_4_toc", File::new(1, 0, 68, 0x900, 0, 11));
    let mut ta = File::new(2, 0, 36, 0x700, 0, 12);
    ta.set(32, 3);
    for (k, (ty, off, size)) in [(1u32, 0u32, 0x400u32), (4, 0x400, 0x200), (5, 0x600, 0x100)].into_iter().enumerate() {
        let at = 36 + k * 16;
        ta.set(at, ty).set(at + 4, 0x1000 + ty).set(at + 8, off).set(at + 12, size);
    }
    add("psp_13_0_4_ta", ta);
    out
}

/// A firmware source over a map (tests, the synthetic set).
pub struct MapSource(pub BTreeMap<String, Vec<u8>>);

impl FwSource for MapSource {
    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, LoadError> {
        Ok(self.0.get(path).cloned())
    }
}

// ---- the whole sequence against the fake (host tests and ktests) ----

/// The microcodes the 780M loads, in Linux's order, as BitsOS runs it (`ListOpts::PSP13_0_4`: no MEC jump
/// table, no MES KIQ, no DMCUB, GX-7d: no VCN), the same for the synthetic and the real files.
pub const EXPECT_780M: &[UcodeId] = &[
    UcodeId::SdmaTh0,
    UcodeId::SdmaTh1,
    UcodeId::CpPfp,
    UcodeId::CpMe,
    UcodeId::CpMec1,
    UcodeId::CpMes,
    UcodeId::CpMesData,
    UcodeId::CpMes1,
    UcodeId::CpMes1Data,
    UcodeId::ImuI,
    UcodeId::ImuD,
    UcodeId::RlcRestoreListGpmMem,
    UcodeId::RlcRestoreListSrmMem,
    UcodeId::RlcIram,
    UcodeId::RlcDram,
    UcodeId::RlcP,
    UcodeId::RlcG,
];

#[derive(Debug)]
pub enum RunError {
    Load(LoadError),
    List(Why),
    Psp(PspError),
}

/// One finished run: what the driver reported, the driver (to stop it) and the BO manager it allocated from.
pub struct Run {
    pub report: Report,
    pub psp: Psp<Mailbox13>,
    pub bo: BoManager,
    pub fw: FwSet,
    pub list: Vec<Ucode>,
}

/// Loads the 780M's files from `src`, makes the load list and runs the PSP bring-up against `fake` with a BO
/// manager over the fake's carve-out. On a PSP error the buffers are given back.
pub fn run(fake: &mut FakeGpu, src: &mut dyn FwSource) -> Result<Run, RunError> {
    let (a, carve, size) = (fake.a, Carve { mc_base: fake.fb_mc, phys_base: fake.fb_phys }, fake.fb_size);
    run_via(fake, a, carve, size, src)
}

/// The same over any `Hw` (a fake behind the bring-up owner or the scanout watcher): PSP mailbox `a`, the
/// carve-out at `carve` with `fb_size` bytes.
pub fn run_via(hw: &mut dyn Hw, a: Addrs, carve: Carve, fb_size: u64, src: &mut dyn FwSource) -> Result<Run, RunError> {
    let opts = ListOpts::PSP13_0_4;
    let (fw, _) = fwload::load(&plan_780m().files(), src, &opts).map_err(RunError::Load)?;
    let list = fw.load_list(&opts).map_err(RunError::List)?;
    let mut bo = BoManager::new(fb_size, Vec::new());
    let mut psp = Psp::new(&mut bo, Mailbox13 { addrs: a }, carve, Timeouts::LINUX).map_err(RunError::Psp)?;
    match psp.start(hw, &mut bo, &fw, &list, opts.autoload) {
        Ok(report) => Ok(Run { report, psp, bo, fw, list }),
        Err(e) => {
            let _ = psp.stop(hw);
            psp.free(&mut bo);
            Err(RunError::Psp(e))
        }
    }
}
