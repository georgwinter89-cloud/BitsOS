//! GX-7i: a fake SDMA 6.0.1 inside the chip fake (`FakeGpu`): SDMA 0's queue registers, the F32's halt, ring 0
//! executed by GR-2's interpreter (`amdgpu_fmt::sim::Engine`) through GFXHUB's VMID 0 (the hub fake: system
//! aperture and GART) into the fake's carve-out and system memory, the rptr writeback, the doorbell (only with
//! NBIO's aperture, the CSDMA range and the queue's doorbell on, else silently dropped as on the chip) and NBIO's
//! RSMU port window. While GC is off its registers are GC's (all ones, writes lost and recorded by the GC fake).
//!
//! Assumptions (not in the capture): the engine runs at once when the write pointer moves (the chip is
//! asynchronous; the stage polls either way), the cold register values are the capture's first reads (F32_CNTL
//! 0x08084001, CNTL 0x2440, RB_CNTL 0x40800, IB_CNTL 0x100, UTCL1_CNTL 0x1f000288, UTCL1_PAGE 0x010cec20), TRAP
//! raises nothing while CNTL's TRAP_ENABLE is 0 (no IH yet), MMIO RB_WPTR moves the queue whether or not the
//! doorbell is on.
//!
//! GX-7i2 (the Mini-PC 28.10.): a taken doorbell sets DOORBELL's CAPTURED, and while CAPTURED is set the queue's
//! pointer registers keep their values (RB_RPTR/RB_WPTR writes are lost: the `again` run kept 0x40/0x40 through a
//! resume that wrote them 0); CAPTURED is cleared by writing it 0 (`captured_w1c`: by writing it 1). A smaller
//! RB_WPTR lands only under MINOR_PTR_UPDATE (sdma_v6_0.c:524). A halted F32 that ran reads TH0_RESET and
//! TH0_ENABLE set (0x08084600).

use super::hubfake::{Access, HubFake, HubHostile};
use super::sdma::{NbioDoorbell, SdmaAddrs, SysAlloc, SysPages};
use crate::ipmod::gmc11::Hub;
use crate::ipmod::sdma6::{R, f};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::bus::{GpuBus, MemFault};
use amdgpu_fmt::sim::{Engine, Event, Isa, Limits, Report, Stop};
use core::cell::RefCell;

const PAGE: u64 = 4096;
/// GFXHUB's client id of SDMA0 (the hub fake's fault decode names it).
pub const CID_SDMA0: u32 = 13;
/// Where the fake's system pages lie (host-physical; far from the carve-out).
pub const SYS_BASE: u64 = 0x2_0000_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SdmaHostile {
    None,
    /// The F32 never executes (a firmware that did not load).
    Dead,
    /// The rptr writeback never lands.
    NoWriteback,
    /// MMIO RB_WPTR does not move the queue (only the doorbell does).
    IgnoreMmioWptr,
    /// Doorbells are dropped.
    NoDoorbell,
    /// FILL_BYTE writes dwords (GR-2's open question answered the other way).
    FillByteIsDword,
    /// Writes into system memory land corrupted (rung 3's copy reads back wrong; its fence still lands).
    CopyCorrupt,
    /// Of a 64-bit fence only the low half lands (dword writes at an address 4 mod 8 are lost: no other dword
    /// write of the stage sits there).
    FenceLowHalfOnly,
    /// The engine refuses INDIRECT (IB depth 0): the golden IB never runs, the ring stops at it.
    IbRefused,
    /// The golden IB runs but its write is lost (its fence lands, the target keeps the poison).
    IbWriteLost,
    /// The engine writes once outside every aperture (GFXHUB records a VM fault); every rung still passes.
    StrayWrite,
    /// GX-7m3: the engine halts at a VM fault (no retry, no default page taken) until its queue is reset.
    FaultStalls,
    /// GX-7i2: RB_RPTR/RB_RPTR_HI writes never land (the other reading of the 28.10. `again` run): a restart must
    /// take the kept read pointer over.
    KeepRptr,
    /// GX-7i2: RB_WPTR/RB_WPTR_HI writes never land (a reset only partly taken: RB_RPTR reads 0, RB_WPTR keeps
    /// the earlier run's value): the restart must fail as StalePointers.
    KeepWptr,
}

/// Sparse system memory by host-physical page (shared with the stage's `SysPages`).
pub type SysMem = Rc<RefCell<BTreeMap<u64, Box<[u8]>>>>;

pub struct SdmaFake {
    pub addrs: Option<SdmaAddrs>,
    regs: BTreeMap<u32, u32>,
    engine: Option<Engine>,
    pub hostile: SdmaHostile,
    /// The F32 ignores HALT (a firmware that does not stop): F32_CNTL keeps HALT clear; RB_ENABLE still applies.
    pub ignore_halt: bool,
    /// DOORBELL's CAPTURED is cleared by writing 1 (else by writing 0).
    pub captured_w1c: bool,
    /// Pointer register writes lost (byte, value).
    pub lost: Vec<(u32, u32)>,
    /// Engine runs and how each ended.
    pub stops: Vec<Stop>,
    pub events: Vec<Event>,
    /// Doorbell writes: (dword index, value, taken).
    pub doorbells: Vec<(u32, u64, bool)>,
    /// NBIO's port registers through RSMU (by index value).
    pub port: BTreeMap<u32, u32>,
    rsmu_index: u32,
    /// Writes into memory by the engine: (host-physical, bytes).
    pub gpu_writes: Vec<(u64, usize)>,
    pub sys: SysMem,
    next_sys: Rc<RefCell<u64>>,
}

impl SdmaFake {
    pub fn new(d: &super::disc::Discovery) -> SdmaFake {
        let addrs = SdmaAddrs::from_discovery(d);
        let mut s = SdmaFake {
            addrs,
            regs: BTreeMap::new(),
            engine: None,
            hostile: SdmaHostile::None,
            ignore_halt: false,
            captured_w1c: false,
            lost: Vec::new(),
            stops: Vec::new(),
            events: Vec::new(),
            doorbells: Vec::new(),
            port: BTreeMap::new(),
            rsmu_index: 0,
            gpu_writes: Vec::new(),
            sys: Rc::new(RefCell::new(BTreeMap::new())),
            next_sys: Rc::new(RefCell::new(SYS_BASE)),
        };
        for (r, v) in [
            (R::F32Cntl, 0x0808_4001),
            (R::Cntl, 0x2440),
            (R::RbCntl, 0x0004_0800),
            (R::IbCntl, 0x100),
            (R::Utcl1Cntl, 0x1f00_0288),
            (R::Utcl1Page, 0x010c_ec20),
            (R::Status, 0x4),
        ] {
            s.set(r, v);
        }
        s
    }

    fn b(&self, r: R) -> Option<u32> {
        self.addrs?.byte(r)
    }

    pub fn set(&mut self, r: R, v: u32) {
        if let Some(b) = self.b(r) {
            self.regs.insert(b, v);
        }
    }

    pub fn get(&self, r: R) -> u32 {
        self.b(r).and_then(|b| self.regs.get(&b).copied()).unwrap_or(0)
    }

    fn nbio(&self) -> Option<NbioDoorbell> {
        self.addrs?.nbio
    }

    /// An SDMA register (lives in GC: gated with it).
    pub fn owns_gc(&self, byte: u32) -> bool {
        self.addrs.is_some_and(|a| R::ALL.into_iter().any(|r| a.byte(r) == Some(byte)))
    }

    /// NBIO's RSMU window (not gated).
    pub fn owns_port(&self, byte: u32) -> bool {
        self.nbio().is_some_and(|n| byte == n.rsmu_index || byte == n.rsmu_data)
    }

    /// The state Linux's load leaves: ring 0 running with its doorbell (the capture's last values).
    pub fn as_linux_left(&mut self) {
        for (r, v) in [(R::F32Cntl, 0x0808_4000), (R::Cntl, 0x1000_2441), (R::RbCntl, 0x0084_1817), (R::IbCntl, 0x101), (R::Doorbell, 0x1000_0000), (R::DoorbellOffset, 0x800), (R::Watchdog, 1), (R::Utcl1Cntl, 0x1f00_0689)] {
            self.set(r, v);
        }
        if let Some(n) = self.nbio() {
            self.port.insert(n.csdma_range, 0x14_0800);
        }
    }

    pub fn running(&self) -> bool {
        self.get(R::F32Cntl) & f::F32_HALT == 0 && self.get(R::RbCntl) & f::RB_ENABLE != 0
    }

    pub fn read(&mut self, byte: u32) -> u32 {
        if let Some(n) = self.nbio()
            && self.owns_port(byte)
        {
            return if byte == n.rsmu_index { self.rsmu_index } else { self.port.get(&self.rsmu_index).copied().unwrap_or(0) };
        }
        if self.b(R::Status) == Some(byte) {
            let idle = self.engine.as_ref().is_none_or(|e| e.rptr() == self.wptr_dw());
            return (self.regs.get(&byte).copied().unwrap_or(0) & !f::STATUS_IDLE) | u32::from(idle);
        }
        self.regs.get(&byte).copied().unwrap_or(0)
    }

    fn wptr_dw(&self) -> u64 {
        (u64::from(self.get(R::RbWptrHi)) << 32 | u64::from(self.get(R::RbWptr))) >> 2
    }

    /// A register write; `true` when the queue should run now.
    pub fn write(&mut self, byte: u32, value: u32) -> bool {
        if let Some(n) = self.nbio()
            && self.owns_port(byte)
        {
            if byte == n.rsmu_index {
                self.rsmu_index = value;
            } else {
                self.port.insert(self.rsmu_index, value);
            }
            return false;
        }
        let value = if self.ignore_halt && self.b(R::F32Cntl) == Some(byte) { value & !f::F32_HALT } else { value };
        let value = if self.b(R::F32Cntl) == Some(byte) && value & f::F32_HALT != 0 && self.get(R::F32Cntl) & f::F32_HALT == 0 {
            value | f::F32_TH0_RESET | f::F32_TH0_ENABLE
        } else {
            value
        };
        let value = if self.b(R::Doorbell) == Some(byte) {
            let had = self.get(R::Doorbell) & f::DOORBELL_CAPTURED;
            let cap = if self.captured_w1c { had & !(value & f::DOORBELL_CAPTURED) } else { value & f::DOORBELL_CAPTURED };
            (value & !f::DOORBELL_CAPTURED) | cap
        } else {
            value
        };
        if self.pointer_lost(byte, value) {
            self.lost.push((byte, value));
            return false;
        }
        // GX-7m3: the queue's reset takes effect at once: a halted engine's ring is forgotten and the bit reads 0.
        if self.b(R::QueueResetReq) == Some(byte) {
            if value & 1 != 0 {
                self.engine = None;
            }
            return false;
        }
        self.regs.insert(byte, value);
        if self.b(R::RbCntl) == Some(byte) {
            if value & f::RB_ENABLE == 0 {
                self.engine = None;
            } else if self.engine.is_none() {
                let base = u64::from(self.get(R::RbBaseHi)) << 40 | u64::from(self.get(R::RbBase)) << 8;
                let size = 1u64 << ((value & f::RB_SIZE_MASK) >> 1);
                let start = (u64::from(self.get(R::RbRptrHi)) << 32 | u64::from(self.get(R::RbRptr))) >> 2;
                let depth = if self.hostile == SdmaHostile::IbRefused { 0 } else { 1 };
                let limits = Limits { max_dw: 1 << 16, max_bytes: 1 << 24, max_ib_depth: depth };
                self.engine = Engine::new(Isa::Sdma, base, size, start, limits).ok();
            }
            return false;
        }
        let wptr = [self.b(R::RbWptr), self.b(R::RbWptrHi)].contains(&Some(byte));
        wptr && self.hostile != SdmaHostile::IgnoreMmioWptr
    }

    /// Whether a write to a ring pointer register is lost (the model in the module comment).
    fn pointer_lost(&self, byte: u32, value: u32) -> bool {
        let is = |r: R| self.b(r) == Some(byte);
        let rptr = is(R::RbRptr) || is(R::RbRptrHi);
        let wptr = is(R::RbWptr) || is(R::RbWptrHi);
        if !rptr && !wptr {
            return false;
        }
        if self.get(R::Doorbell) & f::DOORBELL_CAPTURED != 0 || (rptr && self.hostile == SdmaHostile::KeepRptr) || (wptr && self.hostile == SdmaHostile::KeepWptr) {
            return true;
        }
        let cur = u64::from(self.get(R::RbWptrHi)) << 32 | u64::from(self.get(R::RbWptr));
        let new = if is(R::RbWptr) { cur & !0xffff_ffff | u64::from(value) } else { cur & 0xffff_ffff | u64::from(value) << 32 };
        wptr && new < cur && self.get(R::MinorPtrUpdate) & 1 == 0
    }

    /// A doorbell: taken only with NBIO's aperture (`aper`), the CSDMA range covering it and the queue's doorbell on
    /// at that index; then it is the write pointer (bytes) and the queue runs.
    pub fn doorbell(&mut self, index_dw: u32, value: u64, aper: bool) -> bool {
        let range = self.nbio().and_then(|n| self.port.get(&n.csdma_range).copied()).unwrap_or(0);
        let (off, size) = ((range & 0xffc) >> 2, (range >> 16) & 0xff);
        let q_on = self.get(R::Doorbell) & f::DOORBELL_ENABLE != 0 && (self.get(R::DoorbellOffset) & f::DOORBELL_OFFSET_MASK) >> 2 == index_dw;
        let taken = aper && size > 0 && (off..off + size).contains(&index_dw) && q_on && self.hostile != SdmaHostile::NoDoorbell;
        self.doorbells.push((index_dw, value, taken));
        if taken {
            let db = self.get(R::Doorbell);
            self.set(R::Doorbell, db | f::DOORBELL_CAPTURED);
            self.set(R::RbWptr, value as u32);
            self.set(R::RbWptrHi, (value >> 32) as u32);
        }
        taken
    }

    /// Runs the queue up to the write pointer on `bus`, then the rptr writeback.
    pub fn kick(&mut self, bus: &mut dyn GpuBus) {
        if self.hostile == SdmaHostile::Dead || self.get(R::F32Cntl) & f::F32_HALT != 0 || self.get(R::RbCntl) & f::RB_ENABLE == 0 {
            return;
        }
        let wptr = self.wptr_dw();
        let Some(e) = self.engine.as_mut() else { return };
        let mut last: Option<Report> = None;
        for _ in 0..64 {
            let rep = e.run(&mut *bus, wptr);
            let more = rep.stop == Stop::Budget;
            last = Some(rep);
            if !more {
                break;
            }
        }
        let rptr = e.rptr();
        if self.hostile == SdmaHostile::StrayWrite && self.stops.is_empty() {
            // MC 0x1000 lies below every aperture of the fakes' hubs: a recorded fault, nothing written.
            let _ = bus.write(0, 0x1000, &[0; 4]);
        }
        if let Some(rep) = last {
            self.stops.push(rep.stop);
            self.events.extend(rep.events);
        }
        let hw = rptr << 2;
        self.set(R::RbRptr, hw as u32);
        self.set(R::RbRptrHi, (hw >> 32) as u32);
        if self.get(R::RbCntl) & f::RB_RPTR_WRITEBACK_ENABLE != 0 && self.hostile != SdmaHostile::NoWriteback {
            let at = u64::from(self.get(R::RptrAddrHi)) << 32 | u64::from(self.get(R::RptrAddrLo) & !3);
            let _ = bus.write(0, at, &hw.to_le_bytes());
        }
    }

    /// The stage's system page allocator over this fake's system memory.
    pub fn sys_alloc(&self) -> FakeSys {
        FakeSys { mem: self.sys.clone(), next: self.next_sys.clone() }
    }
}

/// `SysAlloc` for the fakes: pages from `SYS_BASE` up, zeroed, never reused.
pub struct FakeSys {
    mem: SysMem,
    next: Rc<RefCell<u64>>,
}

pub struct FakePages {
    mem: SysMem,
    phys: Vec<u64>,
}

impl SysAlloc for FakeSys {
    fn alloc(&self, pages: u64) -> Result<Box<dyn SysPages>, String> {
        let mut next = self.next.borrow_mut();
        let mut phys = Vec::new();
        for _ in 0..pages {
            phys.push(*next);
            self.mem.borrow_mut().insert(*next / PAGE, vec![0u8; PAGE as usize].into_boxed_slice());
            // A gap after each page: the pages are not contiguous (as DmaPages may give them).
            *next += 2 * PAGE;
        }
        Ok(Box::new(FakePages { mem: self.mem.clone(), phys }))
    }
}

impl SysPages for FakePages {
    fn pages(&self) -> u64 {
        self.phys.len() as u64
    }
    fn device_addr(&self, page: u64) -> Option<u64> {
        self.phys.get(page as usize).copied()
    }
    fn read(&mut self, off: u64, out: &mut [u8]) -> bool {
        let m = self.mem.borrow();
        for (i, b) in out.iter_mut().enumerate() {
            let o = off + i as u64;
            let Some(p) = self.phys.get((o / PAGE) as usize) else { return false };
            let Some(pg) = m.get(&(p / PAGE)) else { return false };
            *b = pg[(o % PAGE) as usize];
        }
        true
    }
    fn write(&mut self, off: u64, data: &[u8]) -> bool {
        let mut m = self.mem.borrow_mut();
        for (i, &b) in data.iter().enumerate() {
            let o = off + i as u64;
            let Some(p) = self.phys.get((o / PAGE) as usize) else { return false };
            let Some(pg) = m.get_mut(&(p / PAGE)) else { return false };
            pg[(o % PAGE) as usize] = b;
        }
        true
    }
}

/// The engine's view of memory: GFXHUB's VMID 0 (`HubFake::translate`), then the carve-out (by MC page in `mem`)
/// or system memory.
pub struct FakeBus<'a> {
    pub hubs: &'a mut HubFake,
    pub mem: &'a mut BTreeMap<u64, Box<[u8]>>,
    pub sys: &'a SysMem,
    pub fb_mc: u64,
    pub fb_phys: u64,
    pub fb_size: u64,
    pub gpu_writes: &'a mut Vec<(u64, usize)>,
    pub hostile: SdmaHostile,
    /// GX-7j: the GFXHUB client the accesses come from (SDMA0 or the CP).
    pub cid: u32,
}

/// 8 bytes of the carve-out at a host-physical address (the hub fake's table reads).
pub fn carve8(mem: &BTreeMap<u64, Box<[u8]>>, fb_mc: u64, fb_phys: u64, fb_size: u64, phys: u64) -> Option<u64> {
    let off = phys.checked_sub(fb_phys).filter(|o| o.checked_add(8).is_some_and(|e| e <= fb_size))?;
    let at = fb_mc + off;
    let mut b = [0u8; 8];
    for (i, x) in b.iter_mut().enumerate() {
        let a = at + i as u64;
        *x = mem.get(&(a / PAGE)).map_or(0, |p| p[(a % PAGE) as usize]);
    }
    Some(u64::from_le_bytes(b))
}

impl FakeBus<'_> {
    fn phys(&mut self, vmid: u8, addr: u64, access: Access) -> Result<u64, MemFault> {
        let write = access == Access::Write;
        // GX-7m2: the CP fetches its commands as CPF, its data as CPG (SDMA is one client for both).
        let cid = if access == Access::Fetch && self.cid == super::gfxfake::CID_CPG { super::gfxfake::CID_CPF } else { self.cid };
        let fault = MemFault { vmid, addr, write };
        let (mem, fb_mc, fb_phys, fb_size) = (&*self.mem, self.fb_mc, self.fb_phys, self.fb_size);
        if vmid != 0 {
            // GX-7m: the VMID's 4-level walk; a fault goes to the default page. GX-7m2: with the permission the access
            // needs (a command fetch needs EXECUTABLE). GX-7m3: whether the client goes on is the client's: CPG drops
            // the access (CP_DEBUG's halt-disable bit, the chip's GFX ring), CPC has no such bit and halts (the chip's
            // compute ring, 30.10.); SDMA is not known yet: it goes on here unless `FaultStalls`.
            let n0 = self.hubs.faults.len();
            let p = self.hubs.translate_vm(Hub::Gfx, vmid, addr, access, cid, &|p| carve8(mem, fb_mc, fb_phys, fb_size, p));
            let halts = match cid {
                super::gfxfake::CID_CPC => self.hubs.hostile != HubHostile::CpcGoesOn,
                super::gfxfake::CID_CPG | super::gfxfake::CID_CPF => self.hubs.hostile == HubHostile::CpgHalts,
                _ => self.hostile == SdmaHostile::FaultStalls,
            };
            return if self.hubs.faults.len() != n0 && halts { Err(fault) } else { p.ok_or(fault) };
        }
        self.hubs.translate(Hub::Gfx, addr, write, cid, &mut |p| carve8(mem, fb_mc, fb_phys, fb_size, p)).ok_or(fault)
    }

    /// One byte at a host-physical address.
    fn byte(&mut self, p: u64, write: Option<u8>) -> bool {
        if p >= self.fb_phys && p < self.fb_phys + self.fb_size {
            let mc = self.fb_mc + (p - self.fb_phys);
            match write {
                Some(v) => {
                    self.mem.entry(mc / PAGE).or_insert_with(|| vec![0u8; PAGE as usize].into_boxed_slice())[(mc % PAGE) as usize] = v;
                    true
                }
                None => true,
            }
        } else {
            let mut m = self.sys.borrow_mut();
            match (m.get_mut(&(p / PAGE)), write) {
                (Some(pg), Some(v)) => {
                    pg[(p % PAGE) as usize] = v;
                    true
                }
                (Some(_), None) => true,
                (None, _) => false,
            }
        }
    }

    fn get(&self, p: u64) -> u8 {
        if p >= self.fb_phys && p < self.fb_phys + self.fb_size {
            let mc = self.fb_mc + (p - self.fb_phys);
            self.mem.get(&(mc / PAGE)).map_or(0, |pg| pg[(mc % PAGE) as usize])
        } else {
            self.sys.borrow().get(&(p / PAGE)).map_or(0, |pg| pg[(p % PAGE) as usize])
        }
    }
}

impl GpuBus for FakeBus<'_> {
    /// GX-7m: an engine reaches GFXHUB's registers and the IH's VMID-to-PASID table (an in-ring VM flush).
    fn reg_read(&mut self, reg: u32) -> Option<u32> {
        self.hubs.ring_read(reg.checked_mul(4)?)
    }
    fn reg_write(&mut self, reg: u32, value: u32) -> bool {
        reg.checked_mul(4).is_some_and(|b| self.hubs.ring_write(b, value))
    }
    fn read(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.load(vmid, addr, out, Access::Read)
    }

    fn fetch(&mut self, vmid: u8, addr: u64, out: &mut [u8]) -> Result<(), MemFault> {
        self.load(vmid, addr, out, Access::Fetch)
    }

    fn write(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        self.store(vmid, addr, data)
    }
}

impl FakeBus<'_> {
    fn load(&mut self, vmid: u8, addr: u64, out: &mut [u8], access: Access) -> Result<(), MemFault> {
        let mut i = 0usize;
        while i < out.len() {
            let a = addr + i as u64;
            let p = self.phys(vmid, a, access)?;
            let n = ((PAGE - (a % PAGE)) as usize).min(out.len() - i);
            for k in 0..n {
                if !self.byte(p + k as u64, None) {
                    return Err(MemFault { vmid, addr: a, write: false });
                }
                out[i + k] = self.get(p + k as u64);
            }
            i += n;
        }
        Ok(())
    }

    fn store(&mut self, vmid: u8, addr: u64, data: &[u8]) -> Result<(), MemFault> {
        // FillByteIsDword: a byte fill (all bytes alike, 4-aligned) lands as dwords of its value.
        let alike = data.len() >= 4 && data.iter().all(|&b| b == data[0]) && data[0] != 0;
        let conv: Vec<u8>;
        let data = if self.hostile == SdmaHostile::FillByteIsDword && alike {
            conv = data.chunks(4).flat_map(|c| [c[0], 0, 0, 0].into_iter().take(c.len())).collect();
            &conv[..]
        } else {
            data
        };
        if self.hostile == SdmaHostile::FenceLowHalfOnly && data.len() == 4 && addr % 8 == 4 {
            return Ok(());
        }
        let golden: Vec<u8> = super::sdma::GOLDEN_RESULT.iter().flat_map(|w| w.to_le_bytes()).collect();
        if self.hostile == SdmaHostile::IbWriteLost && data == &golden[..] {
            return Ok(());
        }
        let mut i = 0usize;
        while i < data.len() {
            let a = addr + i as u64;
            let p = self.phys(vmid, a, Access::Write)?;
            let n = ((PAGE - (a % PAGE)) as usize).min(data.len() - i);
            let sys = !(p >= self.fb_phys && p < self.fb_phys + self.fb_size);
            for k in 0..n {
                let mut v = data[i + k];
                if self.hostile == SdmaHostile::CopyCorrupt && sys && k == 0 {
                    v ^= 0x5a;
                }
                if !self.byte(p + k as u64, Some(v)) {
                    return Err(MemFault { vmid, addr: a, write: true });
                }
            }
            self.gpu_writes.push((p, n));
            i += n;
        }
        Ok(())
    }
}
