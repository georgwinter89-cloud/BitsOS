//! GX-7j: a fake GFX command processor inside the chip fake (`FakeGpu`): CP_ME_CNTL's halts, GFX ring 0's
//! registers (CP_RB0_*, the rptr writeback, the wptr poll address, CP_RB_ACTIVE, the doorbell control and range),
//! ring 0 executed by GR-2's PM4 interpreter (`amdgpu_fmt::sim::Engine`, `Isa::Pm4`) through GFXHUB's VMID 0 (the
//! hub fake: system aperture) into the fake's carve-out, register writes from the ring (SET_UCONFIG_REG,
//! SET_CONTEXT_REG) into the GC fake's registers, RELEASE_MEM through the interpreter. While GC is off its
//! registers are GC's (all ones, writes lost and recorded by the GC fake).
//!
//! Assumptions (no capture of the direct path exists): the CP runs at once when the write pointer moves or it is
//! unhalted (the chip is asynchronous; the stage polls either way); the cold values are the capture's first reads
//! (CP_ME_CNTL 0x15000000: CE, PFP and ME halted; the golden registers, the SA and RB disable masks, TA_CNTL2,
//! GRBM_CNTL, RLC_SRM_CNTL, CP_MEC_CNTL 0x40000000); CP_RB0_RPTR survives every write but CP_RB_RPTR_WR under
//! RB_RPTR_WR_ENA (the conservative reading after SDMA's 28.10. restart); CP_RB_ACTIVE 1 starts the ring at the
//! read pointer; the rptr writeback is the 32-bit dword counter (`RptrWrapped`: the ring offset instead); a
//! doorbell is taken only with NBIO's aperture, DOORBELL_EN, the ring's OFFSET and the index inside the range.

//!
//! GX-7j2: CP_MEC_CNTL (cold 0x40000000, MEC ME1 halted) and CP_STAT after the ME/PFP unhalt. What the chip
//! showed: unhalted while MEC is halted, the PFP stays busy (0x80808000, then 0x80008000: CP_BUSY + PFP_BUSY
//! (+ RCIU)) until the CP is halted again, and fetches nothing; Linux's capture with MEC unhalted first: CP_STAT
//! passes the capture's values and reads 0 after 18 us (`UNHALT_STAT`). Whether an active ring at the unhalt
//! hangs too is unproven: `GfxHostile::ActiveRingHang` stages it. The busy state is decided at the unhalt.

use super::gcfake::GcFake;
use super::gfxring::GfxAddrs;
use super::sdmafake::FakeBus;
use crate::ipmod::gfx11::{self, R, f};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use amdgpu_fmt::bus::{GpuBus, MemFault};
use amdgpu_fmt::sim::{Engine, Event, Isa, Limits, Stop};

/// GFXHUB's client id of the CP's data accesses (CPG: gfxhub_v3_0.c's table index 6; the 780M's MQD fault of
/// 29.10., 0x00000d33, names CID 6). GX-7m2: was 2 (GE1 in that table).
pub const CID_CPG: u32 = 6;
/// GX-7m2: the CP's command fetch (ring and IB dwords): CPF, CID 4, as the 780M named the refused IB fetch of
/// 29.10. (0x00100880).
pub const CID_CPF: u32 = 4;
/// GX-7m3: a compute queue's accesses: CPC, CID 5, as the 780M named the compute ring's fault of 30.10. (0x00140a50).
pub const CID_CPC: u32 = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GfxHostile {
    None,
    /// The CP never fetches the directly programmed ring (the case where MES is needed).
    Dead,
    /// The rptr writeback never lands.
    NoWriteback,
    /// CP_RB0_WPTR does not move the ring (only the doorbell does).
    IgnoreMmioWptr,
    /// Doorbells are dropped.
    NoDoorbell,
    /// The CP refuses INDIRECT_BUFFER (IB depth 0): the IB test never runs.
    IbRefused,
    /// 64-bit RELEASE_MEM writes are lost (the golden IB's and the M5 fence).
    Fence64Lost,
    /// CP_RB_RPTR_WR is ignored: a restart keeps the earlier read pointer.
    IgnoreRptrWr,
    /// The rptr writeback reports the ring offset (rptr mod ring size), not the counter.
    RptrWrapped,
    /// The CP writes once outside every aperture (GFXHUB records a VM fault); every rung still passes.
    StrayWrite,
/// GX-7j2: CP_MEC_CNTL writes are lost (MEC stays halted): the chip's GX-7j failure in every order.
    MecIgnored,
    /// GX-7j2 (hypothesis H1): the PFP also stays busy when ring 0 is active at the unhalt.
    ActiveRingHang,
    /// The PFP stays busy at every unhalt and CP_STAT flips between the stuck values at every poll (the answer
    /// line's bound on the CP_STAT sequence).
    StatFlaps,
    /// GX-7j3: CP_GFX_MQD_BASE_ADDR(_HI) writes are lost: the direct path before GX-7j3 (the pointer stays 0, the
    /// CP reads page 0: the Mini-PC's fault 0x00000d33 of 29.10.).
    MqdPointerLost,
}

/// CP_STAT after an unhalt with MEC running, as the capture read it (us after the CP_ME_CNTL write, value).
pub const UNHALT_STAT: [(u64, u32); 6] = [(0, 0x8802_8000), (3, 0x8002_8000), (5, 0x8802_8000), (13, 0x8082_0000), (15, 0x8000_8000), (18, 0)];
/// CP_STAT of the PFP that never idles (the chip's two GX-7j runs): first with RCIU busy, from `STUCK_SWITCH_US`
/// on without.
pub const STUCK_STAT: (u32, u32) = (0x8080_8000, 0x8000_8000);
pub const STUCK_SWITCH_US: u64 = 10;

pub struct GfxFake {
    pub addrs: Option<GfxAddrs>,
    owned: Vec<u32>,
    regs: BTreeMap<u32, u32>,
    engine: Option<Engine>,
    rptr: u64,
    pub hostile: GfxHostile,
    /// Engine runs and how each ended.
    pub stops: Vec<Stop>,
    pub events: Vec<Event>,
    /// Doorbell writes: (dword index, value, taken).
    pub doorbells: Vec<(u32, u64, bool)>,
    /// Writes into memory by the CP: (host-physical, bytes).
    pub gpu_writes: Vec<(u64, usize)>,
    /// Register writes from the ring (SET_*_REG, WRITE_DATA to a register).
    pub ring_reg_writes: u32,
/// The chip fake's clock (us), set before every access.
    pub now: u64,
    /// When ME/PFP were last unhalted, and whether the PFP stays busy since.
    unhalt_at: Option<u64>,
    stuck: bool,
    /// Every unhalt: (MEC halted then, ring 0 active then, stuck).
    pub unhalts: Vec<(bool, bool, bool)>,
}

/// The CP registers this fake owns (the rest of GC is the GC fake's).
const OWNED: [R; 25] = [
    R::CpMeCntl,
    R::CpMecCntl,
    R::CpRb0Cntl,
    R::CpRb0Wptr,
    R::CpRb0WptrHi,
    R::CpRb0Rptr,
    R::CpRbRptrWr,
    R::CpRb0RptrAddr,
    R::CpRb0RptrAddrHi,
    R::CpRbWptrPollAddrLo,
    R::CpRbWptrPollAddrHi,
    R::CpRb0Base,
    R::CpRb0BaseHi,
    R::CpRbActive,
    R::CpRbDoorbellControl,
    R::CpGfxMqdBaseAddr,
    R::CpGfxMqdBaseAddrHi,
    R::CpGfxMqdControl,
    R::CpRbDoorbellRangeLower,
    R::CpRbDoorbellRangeUpper,
    R::CpMecDoorbellRangeLower,
    R::CpMecDoorbellRangeUpper,
    R::CpRbWptrDelay,
    R::CpRbVmid,
    R::CpMaxContext,
];

/// The capture's first reads of the other GC registers the stage reads (set into the GC fake).
pub const COLD_GC: [(R, u32); 20] = [
    (R::CgttGsNggClkCtrl, 0x10),
    (R::CgttWdClkCtrl, 0x10),
    (R::CpfGcrCntl, 0xc200),
    (R::Gl2cCtrl3, 0xc0d4_0188),
    (R::PaClEnhance, 0x0088_0007),
    (R::PaScEnhance3, 0x8),
    (R::PaScVrsSurfaceCntl1, 0x5548_0100),
    (R::TaCntlAux, 0x0103_0000),
    (R::TcpCntl2, 0x0040_200a),
    (R::TcpCntl, 0x039c_0020),
    (R::GrbmCntl, 0x30),
    (R::CcGcSaUnitDisable, 0x00ff_fc01),
    (R::GcUserSaUnitDisable, 0x00ff_fc00),
    (R::CcRbBackendDisable, 0x1),
    (R::CcGcShaderArrayConfig, 0xfff8_0001),
    (R::CgtsTccDisable, 0x1),
    (R::TaCntl2, 0x0004_0000),
    (R::RlcSrmCntl, 0x3),
    (R::CpMesCntl, 0x4000_0000),
    (R::GrbmStatus, 0x0),
];

impl GfxFake {
    pub fn new(d: &super::disc::Discovery) -> GfxFake {
        let addrs = GfxAddrs::from_discovery(d);
        let owned = addrs.map_or(Vec::new(), |a| OWNED.iter().filter_map(|&r| a.byte(r)).collect());
        let mut g = GfxFake {
            addrs,
            owned,
            regs: BTreeMap::new(),
            engine: None,
            rptr: 0,
            hostile: GfxHostile::None,
            stops: Vec::new(),
            events: Vec::new(),
            doorbells: Vec::new(),
            gpu_writes: Vec::new(),
            ring_reg_writes: 0,
            now: 0,
            unhalt_at: None,
            stuck: false,
            unhalts: Vec::new(),
        };
        g.set(R::CpMeCntl, f::ALL_HALTS);
        g.set(R::CpMecCntl, f::MEC_ME1_HALT);
        g
    }

    /// The capture's cold values of the GC registers around the CP (into the GC fake).
    pub fn cold_gc(&self, gc: &mut GcFake) {
        let Some(a) = self.addrs else { return };
        for (r, v) in COLD_GC {
            if let Some(b) = a.byte(r) {
                gc.set(b, v);
            }
        }
    }

    fn b(&self, r: R) -> Option<u32> {
        self.addrs?.byte(r)
    }

    pub fn owns(&self, byte: u32) -> bool {
        self.owned.contains(&byte)
    }

    pub fn set(&mut self, r: R, v: u32) {
        if let Some(b) = self.b(r) {
            self.regs.insert(b, v);
        }
    }

    pub fn get(&self, r: R) -> u32 {
        self.b(r).and_then(|b| self.regs.get(&b).copied()).unwrap_or(0)
    }

    /// The state Linux's load leaves: the CP unhalted (CE halted), ring 0 MES-mapped; MES runs (the GC fake's
    /// CP_MES_CNTL, set by the caller).
    pub fn as_linux_left(&mut self) {
        self.set(R::CpMeCntl, f::CE_HALT);
    self.set(R::CpMecCntl, 0);
    }

    /// An earlier BitsOS run left ring 0 at `rptr_dw` (halted by its Quiesce hook, the doorbell on).
    pub fn stale_ring(&mut self, rptr_dw: u64) {
        self.rptr = rptr_dw;
        self.set(R::CpRb0Rptr, rptr_dw as u32);
        self.set(R::CpRb0Wptr, rptr_dw as u32);
        self.set(R::CpRbActive, 1);
        self.set(R::CpRbDoorbellControl, gfx11::doorbell_control(0, true));
    }

    pub fn running(&self) -> bool {
        self.get(R::CpMeCntl) & f::GFX_HALTS == 0 && self.engine.is_some()
    }

    /// The PFP spins since the last unhalt (never idle, fetches nothing).
    pub fn stuck(&self) -> bool {
        self.stuck
    }

    pub fn mec_halted(&self) -> bool {
        self.get(R::CpMecCntl) & f::MEC_ME1_HALT != 0
    }

    /// CP_STAT while GC is booted: 0 with ME or PFP halted; after an unhalt the capture's values, or the stuck PFP.
    pub fn cp_stat(&self) -> u32 {
        if self.get(R::CpMeCntl) & f::GFX_HALTS != 0 {
            return 0;
        }
        let el = self.unhalt_at.map_or(u64::MAX, |t| self.now.saturating_sub(t));
        if self.stuck && self.hostile == GfxHostile::StatFlaps {
            let k = if el < 100 { el } else { el / 10 };
            return if k % 2 == 0 { STUCK_STAT.0 } else { STUCK_STAT.1 };
        }
        if self.stuck {
            return if el < STUCK_SWITCH_US { STUCK_STAT.0 } else { STUCK_STAT.1 };
        }
        UNHALT_STAT.iter().rev().find(|x| x.0 <= el).map_or(0, |x| x.1)
    }

    pub fn read(&self, byte: u32) -> u32 {
        if self.b(R::CpRb0Rptr) == Some(byte) {
            return self.rptr as u32;
        }
        self.regs.get(&byte).copied().unwrap_or(0)
    }

    fn wptr(&self) -> u64 {
        u64::from(self.get(R::CpRb0WptrHi)) << 32 | u64::from(self.get(R::CpRb0Wptr))
    }

    fn ring_dw(&self) -> u64 {
        // RB_BUFSZ: the ring is 8 << RB_BUFSZ bytes.
        (8u64 << (self.get(R::CpRb0Cntl) & f::RB_BUFSZ_MASK).min(30)) / 4
    }

    /// A register write; `true` when the ring should run now.
    pub fn write(&mut self, byte: u32, value: u32) -> bool {
        let which = self.addrs.and_then(|a| OWNED.into_iter().find(|&r| a.byte(r) == Some(byte)));
        let is = |r: R| which == Some(r);
        if is(R::CpRb0Rptr) {
            return false; // read-only
        }
        if is(R::CpRbRptrWr) {
            if self.get(R::CpRb0Cntl) & f::RB_RPTR_WR_ENA != 0 && self.hostile != GfxHostile::IgnoreRptrWr {
                self.rptr = u64::from(value);
                if let Some(e) = self.engine.as_mut() {
                    e.reset(self.rptr);
                }
            }
            self.regs.insert(byte, value);
            return false;
        }
        if (is(R::CpGfxMqdBaseAddr) || is(R::CpGfxMqdBaseAddrHi)) && self.hostile == GfxHostile::MqdPointerLost {
            return false;
        }
        let was_halted = self.get(R::CpMeCntl) & f::GFX_HALTS != 0;
        self.regs.insert(byte, value);
        if is(R::CpRbActive) {
            if value & f::RB_ACTIVE == 0 {
                self.engine = None;
            } else {
                let base = (u64::from(self.get(R::CpRb0BaseHi)) << 32 | u64::from(self.get(R::CpRb0Base))) << 8;
                let depth = if self.hostile == GfxHostile::IbRefused { 0 } else { 2 };
                let limits = Limits { max_dw: 1 << 16, max_bytes: 1 << 24, max_ib_depth: depth };
                self.engine = Engine::new(Isa::Pm4, base, self.ring_dw(), self.rptr, limits).ok();
            }
            return false;
        }
        if is(R::CpMecCntl) && self.hostile == GfxHostile::MecIgnored {
            self.regs.insert(byte, f::MEC_ME1_HALT);
            return false;
        }
        if is(R::CpMeCntl) {
            if value & f::GFX_HALTS != 0 {
                self.unhalt_at = None;
                self.stuck = false;
                return false;
            }
            if !was_halted {
                return false;
            }
            let (mec, active) = (self.mec_halted(), self.get(R::CpRbActive) & f::RB_ACTIVE != 0);
            self.stuck = mec || (active && self.hostile == GfxHostile::ActiveRingHang) || self.hostile == GfxHostile::StatFlaps;
            self.unhalt_at = Some(self.now);
            self.unhalts.push((mec, active, self.stuck));
            return !self.stuck;
        }
        let w = is(R::CpRb0Wptr) || is(R::CpRb0WptrHi);
        w && self.hostile != GfxHostile::IgnoreMmioWptr
    }

    /// A doorbell: taken only with NBIO's aperture (`aper`), DOORBELL_EN, the ring's OFFSET at `index_dw` and the
    /// index inside CP_RB_DOORBELL_RANGE_LOWER/UPPER; then it is the write pointer (dwords) and the ring runs.
    pub fn doorbell(&mut self, index_dw: u32, value: u64, aper: bool) -> bool {
        let dc = self.get(R::CpRbDoorbellControl);
        let (lo, hi) = ((self.get(R::CpRbDoorbellRangeLower) & f::DOORBELL_RANGE_MASK) >> 2, (self.get(R::CpRbDoorbellRangeUpper) & f::DOORBELL_RANGE_MASK) >> 2);
        let on = dc & f::DOORBELL_EN != 0 && (dc & f::DOORBELL_OFFSET_MASK) >> 2 == index_dw;
        let taken = aper && on && (lo..=hi).contains(&index_dw) && self.hostile != GfxHostile::NoDoorbell;
        self.doorbells.push((index_dw, value, taken));
        if taken {
            self.set(R::CpRbDoorbellControl, dc | f::DOORBELL_HIT);
            self.set(R::CpRb0Wptr, value as u32);
            self.set(R::CpRb0WptrHi, (value >> 32) as u32);
        }
        taken
    }

    /// Runs the ring up to the write pointer on `bus`, then the rptr writeback.
    pub fn kick(&mut self, bus: &mut dyn GpuBus) {
        if self.hostile == GfxHostile::Dead || self.stuck || self.get(R::CpMeCntl) & f::GFX_HALTS != 0 {
            return;
        }
        let wptr = self.wptr();
        let Some(e) = self.engine.as_mut() else { return };
        let mut last = None;
        for _ in 0..64 {
            let rep = e.run(&mut *bus, wptr);
            let more = rep.stop == Stop::Budget;
            last = Some(rep);
            if !more {
                break;
            }
        }
        self.rptr = e.rptr();
        // GX-7j3, the Mini-PC 29.10.: the RS64 CP reads its active queue's MQD (CP_GFX_MQD_BASE_ADDR, VMID 0) while
        // it runs the ring; the direct path left the pointer 0, and GFXHUB recorded a CPG read fault at page 0
        // (absent on the MES path, whose MQD MES points at). What the CP reads there is not modelled.
        if self.get(R::CpRbActive) & f::RB_ACTIVE != 0 {
            let mqd = u64::from(self.get(R::CpGfxMqdBaseAddrHi)) << 32 | u64::from(self.get(R::CpGfxMqdBaseAddr) & !3);
            let _ = bus.read(0, mqd, &mut [0; 4]);
        }
        if self.hostile == GfxHostile::StrayWrite && self.stops.is_empty() {
            // MC 0x1000 lies below every aperture of the fakes' hubs: a recorded fault, nothing written.
            let _ = bus.write(0, 0x1000, &[0; 4]);
        }
        if let Some(rep) = last {
            self.stops.push(rep.stop);
            self.ring_reg_writes += rep.events.iter().filter(|e| matches!(e, Event::RegWrite { .. })).count() as u32;
            self.events.extend(rep.events.into_iter().filter(|e| !matches!(e, Event::RegWrite { .. })));
        }
        let rb = if self.hostile == GfxHostile::RptrWrapped { self.rptr & (self.ring_dw() - 1) } else { self.rptr };
        if self.get(R::CpRb0Cntl) & f::RB_NO_UPDATE == 0 && self.hostile != GfxHostile::NoWriteback {
            let at = u64::from(self.get(R::CpRb0RptrAddrHi) & f::RPTR_ADDR_HI_MASK) << 32 | u64::from(self.get(R::CpRb0RptrAddr) & !3);
            let _ = bus.write(0, at, &(rb as u32).to_le_bytes());
        }
    }
}

/// The CP's view: memory through `FakeBus` (GFXHUB VMID 0), registers (dword indexes from PM4) into the GC fake.
pub struct GfxBus<'a, 'b> {
    pub mem: FakeBus<'b>,
    pub gc: &'a mut GcFake,
    pub hostile: GfxHostile,
}

impl GpuBus for GfxBus<'_, '_> {
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.mem.read(vmid, addr, out)
    }
    fn fetch(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.mem.fetch(vmid, addr, out)
    }
    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        if self.hostile == GfxHostile::Fence64Lost && data.len() == 8 {
            return Ok(());
        }
        self.mem.write(vmid, addr, data)
    }
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        // GX-7m: GFXHUB's registers and the IH's PASID table first (an in-ring VM flush), then GC's.
        if let Some(v) = self.mem.reg_read(reg) {
            return Some(v);
        }
        let b = reg.checked_mul(4)?;
        self.gc.in_gc(b).then(|| self.gc.read(b)).flatten()
    }
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        if self.mem.reg_write(reg, value) {
            return true;
        }
        let Some(b) = reg.checked_mul(4) else { return false };
        self.gc.in_gc(b) && self.gc.write(b, value)
    }
}
