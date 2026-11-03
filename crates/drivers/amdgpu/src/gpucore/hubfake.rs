//! GX-7h: the two VM hubs in the fakes (gx-scout-bringup.md "hub fakes"): a register file with the values the
//! 780M's firmware leaves (the 27.10. capture's first reads: L1 TLB 0x501, L2 0x80602, every context 0x01fffe00,
//! fault control 0x3ffffffc, CNTL2 GC 0x20000 / MM 0xa0000, CP_DEBUG 0x400000; registers Linux never reads start
//! at 0), invalidation engines (a request sets its VMIDs' ack bits; the semaphore is taken by a read and given back
//! by writing 0, as gmc_v11_0.c:221-235 uses it), and the GPU side of an access (`translate`): VMID 0's GART
//! window through the table in the fake's memory, else the system aperture, else a fault recorded in the
//! L2 protection fault status and address registers as gmc_v11_0.c:96-140 reads them.
//!
//! Assumptions (not in the capture): an unbound GART entry sets PERMISSION_FAULTS bit 0 (the valid bit), an
//! address outside every window sets MAPPING_ERROR; a second fault only sets MORE_FAULTS (no
//! ALLOW_SUBSEQUENT_PROTECTION_FAULT_STATUS_ADDR_UPDATES); the ack comes at once.
//!
//! GX-7m: VMIDs 1-15 walk their 4-level tables (`translate_vm`, GR-1's software walker over the fake's memory).
//! A context's walker uses the page-table base it latched at the last invalidate request naming its VMID (any
//! engine), not the register itself: a base written without the flush is not seen, so a missing in-ring flush
//! shows. A hole, a missing permission or a walk the tables do not carry is a fault with the VMID in the status
//! (PERMISSION_FAULTS valid plus the access bit for a hole, read 0x3 / write 0x5; for a mapped page the missing
//! permissions (GX-7m2): a read needs READABLE (0x2), a write WRITEABLE (0x4), a command fetch (`Access::Fetch`,
//! ring or IB dwords) READABLE and EXECUTABLE (0x2 | 0x8): the 780M's CPF fetch of an IB page mapped without
//! EXECUTABLE read 0x00100880 on 29.10. (PERMISSION_FAULTS 0x8, a read). Assumed, not seen: SDMA's fetch needs
//! EXECUTABLE too, and a fetch of a hole reads like a read (0x3); WALKER_ERROR 1 for an unreadable table;
//! MAPPING_ERROR outside the context's range), and the access goes to the protection-fault default page when the
//! context has its fault defaults on (gfxhub_v3_0 setup_vmid_config): the client goes on, as on the chip with
//! retry off. The IH's VMID-to-PASID table (IH_VMID_n_LUT, OSSSYS) lives here too: the fault path reads it with
//! the hub registers and the rings write it beside them (gmc_v11_0_emit_pasid_mapping).

use super::disc::Discovery;
use super::gmc::HubAddrs;
use crate::ipmod::gmc11::{CONTEXTS, ENGINES, Hub, R, f};
use crate::ipmod::ih6;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use amdgpu_fmt::store::TableRead;
use amdgpu_fmt::walk::{self, Walk};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HubHostile {
    None,
    /// The hub never acks a flush.
    NoAck(Hub),
    /// The engine's semaphore is never free.
    SemBusy(Hub),
    /// GX-7m: CLEAR_PROTECTION_FAULT_STATUS_ADDR does not clear the status (a fault that stays).
    NoClear(Hub),
    /// GX-7m: GFXHUB's invalidation engine `n` never acks (a ring's own engine; the CPU's 17 still does).
    NoAckEngine(u8),
    /// GX-7m: a ring's (or MES's) write of a context's page-table base is lost (the CPU's still lands).
    RingBaseLost,
    /// GX-7m2: every PTE the walker finds reads without EXECUTABLE (the 29.10. tables): a command fetch faults.
    NoExecute,
    /// GX-7m3: CPC drops a faulting write and goes on, as CPG (not what the 780M does: the chip's CPC halts).
    CpcGoesOn,
    /// GX-7m3: CPG halts at a fault as CPC does (CP_DEBUG's halt-disable bit lost): GFX ring 0 must not survive that.
    CpgHalts,
    /// GX-7m3: a VM fault reaches no status register (a client halts and nothing says why).
    FaultUnrecorded,
}

/// GX-7m2: what an engine does at a VM address (the permission it needs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    /// Command dwords (a ring or an IB): READABLE and EXECUTABLE; the hub reports it as a read.
    Fetch,
}

/// A recorded GPU access that faulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Faulted {
    pub hub: Hub,
    pub mc: u64,
    pub status: u32,
    /// GX-7m: the VMID of the access (0: the GART/system aperture path).
    pub vmid: u8,
}

pub struct HubFake {
    pub gfx: Option<HubAddrs>,
    pub mm: Option<HubAddrs>,
    /// byte -> (hub, register) for every register the fake models.
    at: BTreeMap<u32, (Hub, R)>,
    regs: BTreeMap<u32, u32>,
    held: BTreeMap<(Hub, u8), bool>,
    acks: BTreeMap<(Hub, u8), u32>,
    pub hostile: HubHostile,
    /// Every invalidate request: (hub, engine, value).
    pub requests: Vec<(Hub, u8, u32)>,
    /// Semaphore reads that took it.
    pub sem_taken: u32,
    pub faults: Vec<Faulted>,
    /// Every hub register read through `read` (the live owner's read list must hold them all).
    pub read_set: alloc::collections::BTreeSet<u32>,
    /// GX-7m: the page-table base each VMID's walker latched at its last invalidate (index = VMID).
    /// (On the heap: the fakes live on ktest stacks.)
    latched: Vec<Option<u64>>,
    /// GX-7m: IH_VMID_0_LUT's byte (OSSSYS segment 0) and the 16 entries.
    lut_base: Option<u32>,
    lut: Vec<u32>,
    /// GX-7m: every IH_VMID_n_LUT write: (VMID, PASID).
    pub lut_writes: Vec<(u8, u32)>,
}

/// GR-1's walker reads the fake's memory through this (8 bytes at a host-physical address).
struct Tables<'a>(&'a dyn Fn(u64) -> Option<u64>);

impl TableRead for Tables<'_> {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        (self.0)(table.checked_add(index as u64 * 8)?)
    }
}

fn all_regs() -> Vec<R> {
    let mut v = alloc::vec![
        R::FbOffset, R::FbBase, R::FbTop, R::AgpTop, R::AgpBot, R::AgpBase, R::ApertLow, R::ApertHigh, R::DefaultLsb, R::DefaultMsb, R::L1Tlb,
        R::L2Cntl, R::L2Cntl2, R::L2Cntl3, R::L2Cntl4, R::L2Cntl5, R::FaultCntl, R::FaultCntl2, R::FaultStatus, R::FaultAddrLo, R::FaultAddrHi,
        R::FaultDefaultLo, R::FaultDefaultHi, R::IdentLowLo, R::IdentLowHi, R::IdentHighLo, R::IdentHighHi, R::IdentOffLo, R::IdentOffHi, R::CpDebug,
    ];
    for n in 0..CONTEXTS {
        v.extend([R::Ctx(n), R::PtBaseLo(n), R::PtBaseHi(n), R::PtStartLo(n), R::PtStartHi(n), R::PtEndLo(n), R::PtEndHi(n)]);
    }
    for e in 0..ENGINES {
        v.extend([R::Sem(e), R::Req(e), R::Ack(e), R::RangeLo(e), R::RangeHi(e)]);
    }
    v
}

impl HubFake {
    pub fn new(d: &Discovery) -> HubFake {
        let mut h = HubFake {
            gfx: HubAddrs::from_discovery(d, Hub::Gfx),
            mm: HubAddrs::from_discovery(d, Hub::Mm),
            at: BTreeMap::new(),
            regs: BTreeMap::new(),
            held: BTreeMap::new(),
            acks: BTreeMap::new(),
            hostile: HubHostile::None,
            requests: Vec::new(),
            sem_taken: 0,
            faults: Vec::new(),
            read_set: alloc::collections::BTreeSet::new(),
            latched: alloc::vec![None; CONTEXTS as usize],
            lut_base: d.reg(super::hwid::OSSSYS, 0, ih6::IH_VMID_0_LUT.0, ih6::IH_VMID_0_LUT.1).and_then(|x| x.checked_mul(4)),
            lut: alloc::vec![0; CONTEXTS as usize],
            lut_writes: Vec::new(),
        };
        for a in [h.gfx, h.mm].into_iter().flatten() {
            for r in all_regs() {
                if let Some(b) = a.byte(r) {
                    h.at.insert(b, (a.hub, r));
                }
            }
        }
        for hub in [Hub::Gfx, Hub::Mm] {
            let mut cold = alloc::vec![(R::L1Tlb, 0x501), (R::L2Cntl, 0x8_0602), (R::FaultCntl, 0x3fff_fffc)];
            cold.push((R::FaultCntl2, if hub == Hub::Gfx { 0x2_0000 } else { 0xa_0000 }));
            cold.push((R::CpDebug, 0x40_0000));
            cold.extend((0..CONTEXTS).map(|n| (R::Ctx(n), 0x01ff_fe00)));
            for (r, v) in cold {
                h.set_r(hub, r, v);
            }
        }
        h
    }

    fn addrs(&self, hub: Hub) -> Option<HubAddrs> {
        if hub == Hub::Gfx { self.gfx } else { self.mm }
    }

    /// The hub that owns `byte` (a register this fake models).
    pub fn owns(&self, byte: u32) -> Option<Hub> {
        self.at.get(&byte).map(|&(h, _)| h)
    }

    pub fn set(&mut self, byte: u32, value: u32) {
        self.regs.insert(byte, value);
    }

    pub fn set_r(&mut self, hub: Hub, r: R, value: u32) {
        if let Some(b) = self.addrs(hub).and_then(|a| a.byte(r)) {
            self.regs.insert(b, value);
        }
    }

    pub fn get(&self, hub: Hub, r: R) -> u32 {
        self.addrs(hub).and_then(|a| a.byte(r)).and_then(|b| self.regs.get(&b).copied()).unwrap_or(0)
    }

    pub fn sem_held(&self, hub: Hub, e: u8) -> bool {
        self.held.get(&(hub, e)).copied().unwrap_or(false)
    }

    /// A register read (`None`: not a hub register).
    pub fn read(&mut self, byte: u32) -> Option<u32> {
        let &(hub, r) = self.at.get(&byte)?;
        self.read_set.insert(byte);
        Some(match r {
            R::Sem(e) => {
                if self.hostile == HubHostile::SemBusy(hub) || self.sem_held(hub, e) {
                    0
                } else {
                    self.held.insert((hub, e), true);
                    self.sem_taken += 1;
                    1
                }
            }
            R::Ack(e) => self.acks.get(&(hub, e)).copied().unwrap_or(0),
            _ => self.regs.get(&byte).copied().unwrap_or(0),
        })
    }

    /// A register write (`false`: not a hub register).
    pub fn write(&mut self, byte: u32, value: u32) -> bool {
        let Some(&(hub, r)) = self.at.get(&byte) else { return false };
        match r {
            R::Sem(e) => {
                if value & 1 == 0 {
                    self.held.insert((hub, e), false);
                }
            }
            R::Req(e) => {
                self.requests.push((hub, e, value));
                let dead = self.hostile == HubHostile::NoAck(hub) || (hub == Hub::Gfx && self.hostile == HubHostile::NoAckEngine(e));
                let ack = if dead { 0 } else { value & 0xffff };
                self.acks.insert((hub, e), ack);
                // GX-7m: the walkers of the VMIDs the request names take their context's base now.
                if hub == Hub::Gfx && ack != 0 {
                    for n in 1..CONTEXTS {
                        if ack >> n & 1 != 0 {
                            let base = u64::from(self.get(hub, R::PtBaseLo(n))) | u64::from(self.get(hub, R::PtBaseHi(n))) << 32;
                            self.latched[n as usize] = Some(base);
                        }
                    }
                }
            }
            R::FaultCntl if value & f::FAULT_CLEAR_STATUS != 0 => {
                if self.hostile != HubHostile::NoClear(hub) {
                    for x in [R::FaultStatus, R::FaultAddrLo, R::FaultAddrHi] {
                        self.set_r(hub, x, 0);
                    }
                }
                self.regs.insert(byte, value & !f::FAULT_CLEAR_STATUS);
            }
            _ => {
                self.regs.insert(byte, value);
            }
        }
        true
    }

    fn fault(&mut self, hub: Hub, mc: u64, write: bool, cid: u32, kind: u32) {
        self.fault_vm(hub, 0, mc, write, cid, kind);
    }

    fn fault_vm(&mut self, hub: Hub, vmid: u8, mc: u64, write: bool, cid: u32, kind: u32) {
        let old = self.get(hub, R::FaultStatus);
        let status = if old != 0 { old | 1 } else { kind | (cid & 0x1ff) << 9 | u32::from(write) << 18 | u32::from(vmid & 0xf) << 20 };
        if self.hostile == HubHostile::FaultUnrecorded {
            self.faults.push(Faulted { hub, mc, status, vmid });
            return;
        }
        if old == 0 {
            self.set_r(hub, R::FaultAddrLo, (mc >> 12) as u32);
            self.set_r(hub, R::FaultAddrHi, (mc >> 44) as u32 & 0xf);
        }
        self.set_r(hub, R::FaultStatus, status);
        self.faults.push(Faulted { hub, mc, status, vmid });
    }

    /// GX-7m: the byte of IH_VMID_n_LUT, when the discovery table has OSSSYS.
    pub fn lut_byte(&self, vmid: u8) -> Option<u32> {
        self.lut_base?.checked_add(u32::from(vmid) * 4).filter(|_| vmid < CONTEXTS)
    }

    /// GX-7m: whether `byte` is one of the IH's VMID-to-PASID entries.
    pub fn lut_owns(&self, byte: u32) -> bool {
        self.lut_index(byte).is_some()
    }

    fn lut_index(&self, byte: u32) -> Option<usize> {
        let o = byte.checked_sub(self.lut_base?)?;
        (o % 4 == 0 && o / 4 < u32::from(CONTEXTS)).then_some((o / 4) as usize)
    }

    /// GX-7m: an IH_VMID_n_LUT entry (CPU or ring).
    pub fn lut_read(&self, byte: u32) -> Option<u32> {
        self.lut_index(byte).map(|i| self.lut[i])
    }

    pub fn lut_write(&mut self, byte: u32, value: u32) -> bool {
        let Some(i) = self.lut_index(byte) else { return false };
        self.lut[i] = value;
        self.lut_writes.push((i as u8, value));
        true
    }

    /// GX-7m: a register as an engine (a ring's WRITE_DATA/SRBM_WRITE, a WAIT_REG_MEM poll) reads it: a hub
    /// register or a LUT entry; `None` when neither.
    pub fn ring_read(&mut self, byte: u32) -> Option<u32> {
        if self.lut_owns(byte) {
            return self.lut_read(byte);
        }
        self.read(byte)
    }

    pub fn ring_write(&mut self, byte: u32, value: u32) -> bool {
        if self.hostile == HubHostile::RingBaseLost && matches!(self.at.get(&byte), Some((_, R::PtBaseLo(_) | R::PtBaseHi(_)))) {
            return true;
        }
        self.lut_write(byte, value) || self.write(byte, value)
    }

    /// GX-7m: the base VMID `vmid`'s walker uses (latched at its last invalidate).
    pub fn latched(&self, vmid: u8) -> Option<u64> {
        self.latched.get(vmid as usize).copied().flatten()
    }

    /// GX-7m: an access of client `cid` through GFXHUB in VMID `vmid` (1-15): the 4-level walk from the latched
    /// base. `Some(phys)`: the page, or the default page after a recorded fault (the context's fault defaults
    /// on); `None`: a fault with no default page. `mem` reads 8 bytes at a host-physical address.
    pub fn translate_vm(&mut self, hub: Hub, vmid: u8, va: u64, access: Access, cid: u32, mem: &dyn Fn(u64) -> Option<u64>) -> Option<u64> {
        let n = vmid & 0xf;
        let g = |s: &Self, r: R| u64::from(s.get(hub, r));
        let ctx = g(self, R::Ctx(n)) as u32;
        let page = (va & amdgpu_fmt::addr::HOLE_MASK) >> 12;
        let start = g(self, R::PtStartLo(n)) | g(self, R::PtStartHi(n)) << 32;
        let end = g(self, R::PtEndLo(n)) | g(self, R::PtEndHi(n)) << 32;
        let write = access == Access::Write;
        let kind = if ctx & f::CTX_ENABLE == 0 || !(start..=end).contains(&page) {
            1 << 8
        } else {
            let root = self.latched(n).unwrap_or(0) & 0x0000_ffff_ffff_f000;
            match walk::walk(&Tables(mem), root, amdgpu_fmt::addr::sign_extend(va & amdgpu_fmt::addr::HOLE_MASK)) {
                Ok(Walk::Page(l)) => {
                    let exec = l.flags.exec && self.hostile != HubHostile::NoExecute;
                    let missing = match access {
                        Access::Read => u32::from(!l.flags.read) << 1,
                        Access::Write => u32::from(!l.flags.write) << 2,
                        Access::Fetch => u32::from(!l.flags.read) << 1 | u32::from(!exec) << 3,
                    };
                    if missing == 0 {
                        return Some(l.phys);
                    }
                    missing << 4
                }
                Ok(Walk::Prt { .. }) | Ok(Walk::Hole { .. }) => (if write { 0x5 } else { 0x3 }) << 4,
                Err(_) => 1 << 1,
            }
        };
        self.fault_vm(hub, n, va, write, cid, kind);
        let defaults = ctx & f::CTX_FAULT_DEFAULTS != 0 && g(self, R::FaultCntl) as u32 & f::FAULT_ENABLE_DEFAULTS != 0;
        defaults.then(|| (g(self, R::FaultDefaultLo) << 12 | (g(self, R::FaultDefaultHi) & 0xf) << 44) | (va & 0xfff))
    }

    /// The GPU's view of an access by client `cid` through `hub` on VMID 0: the host-physical address, or a
    /// recorded fault (`None`). `mem` reads 8 bytes of graphics memory at a host-physical address.
    pub fn translate(&mut self, hub: Hub, mc: u64, write: bool, cid: u32, mem: &mut dyn FnMut(u64) -> Option<u64>) -> Option<u64> {
        let g = |s: &Self, r: R| u64::from(s.get(hub, r));
        let page = mc >> 12;
        let ctx0 = g(self, R::Ctx(0)) as u32;
        let start = g(self, R::PtStartLo(0)) | g(self, R::PtStartHi(0)) << 32;
        let end = g(self, R::PtEndLo(0)) | g(self, R::PtEndHi(0)) << 32;
        if ctx0 & f::CTX_ENABLE != 0 && (start..=end).contains(&page) {
            let base = (g(self, R::PtBaseLo(0)) | g(self, R::PtBaseHi(0)) << 32) & 0x0000_ffff_ffff_f000;
            let e = mem(base + (page - start) * 8);
            return match e {
                Some(e) if e & 1 != 0 => Some((e & 0x0000_ffff_ffff_f000) | (mc & 0xfff)),
                _ => {
                    self.fault(hub, mc, write, cid, 1 << 4);
                    None
                }
            };
        }
        let fb_start = (g(self, R::FbBase) & 0xff_ffff) << 24;
        let fb_end = (g(self, R::FbTop) & 0xff_ffff) << 24 | 0xff_ffff;
        let low = g(self, R::ApertLow) << 18;
        let high = g(self, R::ApertHigh) << 18 | 0x3_ffff;
        if (low..=high).contains(&mc) && (fb_start..=fb_end).contains(&mc) {
            return Some(mc - fb_start + ((g(self, R::FbOffset) & 0xff_ffff) << 24));
        }
        self.fault(hub, mc, write, cid, 1 << 8);
        None
    }
}
