//! GX-8v: programs' VMIDs on MMHUB for VCN, the GX-D9 way (plan/spec/gpu.md 11.2). Linux runs a
//! program's decode job with the job's VMID on MMHUB (vcn_v2_0_enc_ring_emit_ib), flushing that VMID in the ring
//! first (vcn_v2_0_enc_ring_emit_vm_flush -> gmc_v11_0_emit_flush_gpu_tlb: semaphore, the context's page-table base,
//! the invalidate request and its ack, the semaphore back, a wait on the base), then naming the job's PASID in the
//! IH's MMHUB table (amdgpu_vm_flush -> gmc_v11_0_emit_pasid_mapping: IH_VMID_n_LUT_MM). What that needs of MMHUB:
//! - CPU writes, once: VM_CONTEXT1..7_CNTL and their PAGE_TABLE_START/END as `mmhub_v3_0_1_setup_vmid_config` writes
//!   them (`gmc11::ctxn_cntl`, 0 .. MAX_PFN), and engine 0's ADDR_RANGE_LO32/HI32 as
//!   `mmhub_v3_0_1_program_invalidation` writes them (all addresses: without it the in-ring flush may invalidate
//!   nothing, and a VMID's second job could run on stale translations). Only engine 0, the one VCN's ring uses; the
//!   other 17 engines no client of ours flushes. None of these is a field the display's fetch uses (the watcher's
//!   23-field list): each is checked against the guard before it is written, the stage refuses otherwise, and a
//!   register already holding Linux's value is not written.
//! - Ring writes (the VCPU's REG_WRITE, never the CPU's): the contexts' base and the LUT_MM entry for VMIDs 1-7,
//!   engine 0's request and semaphore. `ring_ok` checks every frame against exactly that list, and the request's and
//!   semaphore's values too: a request names exactly one of VMIDs 1-7 (never VMID 0), the semaphore is only given back.
//! MMHUB's L2 stays as the firmware left it (ENABLE_L2_CACHE 0, MX_L1 SYSTEM_ACCESS_MODE 0, both display fields):
//! whether the hub translates VMIDs 1-15 like that is unmeasured (no probe runs at boot: Georg 18.11.; a fault shows
//! in MMHUB's status, which the job half reads each round).

use super::bringup::{Bringup, Outcome as WriteOutcome};
use super::gmc::HubAddrs;
use super::psp::Hw;
use super::watch::{DisplayGuard, Watch};
use crate::ipmod::gmc11::{self, Hub, R};
use alloc::vec::Vec;
use amdgpu_fmt::vcn::{self as pkt, Packet};

/// The VMIDs a VCN job may carry: the kernel's (hw::vm::KERNEL_VMIDS; MES owns 8-15).
pub const VMIDS: core::ops::Range<u8> = 1..8;
/// VCN 0's invalidation engine on MMHUB (Linux's dmesg: "ring vcn_unified_0 uses VM inv eng 0 on hub 8").
pub const ENGINE: u8 = 0;

/// One context register of the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ctx {
    pub r: R,
    pub byte: u32,
    pub before: u32,
    pub value: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CtxError {
    NoAddress,
    Read { byte: u32 },
    /// The guard calls this register the display's: never written (GX-D9).
    Display { byte: u32 },
    Write { byte: u32 },
}

fn regs(n: u8) -> [(R, fn(u32) -> u32); 5] {
    [
        (R::Ctx(n), gmc11::ctxn_cntl),
        (R::PtStartLo(n), |_| 0),
        (R::PtStartHi(n), |_| 0),
        (R::PtEndLo(n), |_| gmc11::MAX_PFN_LAST as u32),
        (R::PtEndHi(n), |_| (gmc11::MAX_PFN_LAST >> 32) as u32),
    ]
}

/// `program_invalidation` for engine 0: the whole address range.
fn engine_regs() -> [(R, fn(u32) -> u32); 2] {
    [(R::RangeLo(ENGINE), |_| gmc11::RANGE_LO_ALL), (R::RangeHi(ENGINE), |_| gmc11::RANGE_HI_ALL)]
}

fn all_regs() -> impl Iterator<Item = (R, fn(u32) -> u32)> {
    VMIDS.flat_map(regs).chain(engine_regs())
}

/// What `setup_vmid_config` writes for VMIDs 1-7 on MMHUB and `program_invalidation` for engine 0, from the values
/// read.
pub fn plan(hw: &mut dyn Hw, mm: &HubAddrs, guard: &DisplayGuard) -> Result<Vec<Ctx>, CtxError> {
    if mm.hub != Hub::Mm {
        return Err(CtxError::NoAddress);
    }
    let mut v = Vec::new();
    for (r, f) in all_regs() {
        let byte = mm.byte(r).ok_or(CtxError::NoAddress)?;
        if guard.class(byte).is_some() {
            return Err(CtxError::Display { byte });
        }
        let before = hw.reg_read(byte).ok_or(CtxError::Read { byte })?;
        v.push(Ctx { r, byte, before, value: f(before) });
    }
    Ok(v)
}

/// Every register `plan` may write (the live owner's list).
pub fn writable(mm: &HubAddrs) -> Vec<u32> {
    all_regs().filter_map(|(r, _)| mm.byte(r)).collect()
}

/// Writes the plan's differing registers through the owner; returns how many.
pub fn enable<H: Hw>(owner: &mut Bringup<Watch<H>>, mm: &HubAddrs, guard: &DisplayGuard) -> Result<Vec<Ctx>, CtxError> {
    let p = plan(owner, mm, guard)?;
    let mut written = Vec::new();
    for c in p.into_iter().filter(|c| c.before != c.value) {
        match owner.write(c.byte, c.value).outcome {
            WriteOutcome::Ok | WriteOutcome::Mismatch => written.push(c),
            _ => return Err(CtxError::Write { byte: c.byte }),
        }
    }
    klog::event!(Info, "gx", "vcn.mm_contexts", written = written.len(); "MMHUB VMIDs 1-7 and engine 0's range for VCN's jobs (no display field)");
    Ok(written)
}

/// What a VCN frame may write (dwords): the contexts' bases and LUT_MM entries for `VMIDS`, engine 0's request and
/// semaphore (`ring_ok` checks those two by value as well).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RingList {
    pub regs: Vec<u32>,
    req: Option<u32>,
    sem: Option<u32>,
}

/// `lut_mm`: IH_VMID_0_LUT_MM's byte (`VcnAddrs::lut_mm`).
pub fn ring_writable(mm: &HubAddrs, lut_mm: Option<u32>) -> RingList {
    let dw = |r: R| mm.byte(r).map(|b| b / 4);
    let mut regs: Vec<u32> = VMIDS.flat_map(|n| [R::PtBaseLo(n), R::PtBaseHi(n)]).filter_map(dw).collect();
    regs.extend(VMIDS.filter_map(|n| lut_dw(lut_mm, n)));
    let (req, sem) = (dw(R::Req(ENGINE)), dw(R::Sem(ENGINE)));
    regs.extend(req.into_iter().chain(sem));
    RingList { regs, req, sem }
}

fn lut_dw(lut_mm: Option<u32>, vmid: u8) -> Option<u32> {
    lut_mm?.checked_add(u32::from(vmid) * 4).map(|b| b / 4)
}

/// The VMIDs a request may name (bits 1-7; bit 0 is VMID 0, the kernel's context).
const REQ_VMIDS: u32 = 0xfe;

impl RingList {
    /// A value the frame may write to `reg_dw`: a flush request names exactly one program VMID, the semaphore is only
    /// given back (0).
    fn value_ok(&self, reg_dw: u32, value: u32) -> bool {
        if Some(reg_dw) == self.req {
            let ids = value & 0xffff;
            return ids.count_ones() == 1 && ids & !REQ_VMIDS == 0;
        }
        if Some(reg_dw) == self.sem {
            return value == 0;
        }
        true
    }
}

/// The in-ring flush of `vmid` to `pd` (gmc_v11_0_emit_flush_gpu_tlb on MMHUB, with its semaphore, then VCN's wait on
/// the base: vcn_v2_0_enc_ring_emit_vm_flush). `None`: no such VMID or register.
pub fn flush(mm: &HubAddrs, vmid: u8, pd: u64) -> Option<Vec<u32>> {
    if !VMIDS.contains(&vmid) {
        return None;
    }
    let dw = |r: R| mm.byte(r).map(|b| b / 4);
    let (sem, req, ack) = (dw(R::Sem(ENGINE))?, dw(R::Req(ENGINE))?, dw(R::Ack(ENGINE))?);
    let (lo, hi) = (dw(R::PtBaseLo(vmid))?, dw(R::PtBaseHi(vmid))?);
    let mask = 1u32 << vmid;
    let mut v = Vec::new();
    pkt::reg_wait(&mut v, sem, 1, 1);
    pkt::reg_write(&mut v, lo, pd as u32);
    pkt::reg_write(&mut v, hi, (pd >> 32) as u32);
    // amdgpu_ring_emit_reg_write_reg_wait_helper: the request, then the ack.
    pkt::reg_write(&mut v, req, gmc11::invalidate_req(vmid, 0));
    pkt::reg_wait(&mut v, ack, mask, mask);
    pkt::reg_write(&mut v, sem, 0);
    pkt::reg_wait(&mut v, lo, 0xffff_ffff, pd as u32);
    Some(v)
}

/// The job's PASID in IH_VMID_`vmid`_LUT_MM (gmc_v11_0_emit_pasid_mapping on an MMHUB ring, after the flush as
/// amdgpu_vm_flush emits it): an MMHUB fault of the job names its process. Nothing without the address.
pub fn pasid_map(lut_mm: Option<u32>, vmid: u8, pasid: u32) -> Vec<u32> {
    let mut v = Vec::new();
    if VMIDS.contains(&vmid)
        && let Some(r) = lut_dw(lut_mm, vmid)
    {
        pkt::reg_write(&mut v, r, pasid);
    }
    v
}

/// Every REG_WRITE of `words` is on `allowed` with a value it allows, and nothing in it is not a packet the frame
/// builds.
pub fn ring_ok(words: &[u32], allowed: &RingList) -> bool {
    let mut i = 0;
    while i < words.len() {
        let Some((p, n)) = Packet::decode(&words[i..]) else { return false };
        match p {
            Packet::RegWrite { reg_dw, value } if !allowed.regs.contains(&reg_dw) || !allowed.value_ok(reg_dw, value) => return false,
            Packet::Unknown(_) => return false,
            _ => {}
        }
        i += n;
    }
    true
}
