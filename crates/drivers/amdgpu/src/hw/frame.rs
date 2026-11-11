//! GX-8b: one job's frame on a ring, in the order of `amdgpu_ib_schedule` (amdgpu_ib.c:126) as it comes out for a
//! kernel queue of GC 11.0.1 on an APU:
//!
//! 1. `emit_mem_sync` when the first IB asks (:206): ACQUIRE_MEM (gfx_v11_0.c:6039), SDMA's GCR_REQ (sdma_v6_0.c:282);
//! 2. `amdgpu_vm_flush` (amdgpu_vm.c:599): the pipeline sync, the flush with its PFP_SYNC_ME, the PASID entry;
//! 3. no shadow: `emit_gfx_shadow` is empty without `cp_gfx_shadow`, which only SR-IOV on 11.0.0/2/3 sets (gfx_v11_0.c:466);
//! 4. no HDP flush or invalidate: `amdgpu_device_flush_hdp` returns at once on an x86-64 APU (amdgpu_device.c:5857);
//! 5. CONTEXT_CONTROL and FRAME_CONTROL, GFX only (gfx_v11_0.c:5460, :5628); never TMZ, `Jobs::submit` refuses it;
//! 6. the IBs, the frame's end;
//! 7. the fence: RELEASE_MEM with Linux's cache bits, SDMA's FENCE.
//!
//! What Linux has and this does not, each with no effect on this chip: COND_EXEC around the flush and the IBs (its slot
//! only goes to 0 for MCBP's preemption, and `gfx.mcbp` is off here, amdgpu_device.c:3473, so no DE meta either); the
//! extra fence after a flush (Linux times `id->last_flush` with it, here the job's own fence does: a later signal costs
//! one flush too many at most); a GDS switch (no GEM object can be in GDS, so a job's sizes stay 0). The user fence is
//! the CPU's: see `run::finish`. The SDMA flush is `vmring::sdma_flush`, the chip's proven sequence.

use crate::gpucore::vmring::{self, AckWait, Q, VmAddrs, VmError};
use alloc::vec::Vec;
use amdgpu_fmt::addr::sign_extend;
use amdgpu_fmt::pm4::{self, WaitRegMem};
use amdgpu_fmt::ring::{Ring, RingError};
use amdgpu_fmt::sdma as spkt;
use drm::job::JobIb;
use drm::uapi::*;
use linux::errno::EINVAL;
use vfs::Errno;

/// A kernel queue the backend runs jobs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingType {
    Gfx,
    Compute,
    Sdma,
}

impl RingType {
    pub const ALL: [RingType; 3] = [RingType::Gfx, RingType::Compute, RingType::Sdma];

    pub fn of_ip(ip: u32) -> Option<RingType> {
        match ip {
            AMDGPU_HW_IP_GFX => Some(RingType::Gfx),
            AMDGPU_HW_IP_COMPUTE => Some(RingType::Compute),
            AMDGPU_HW_IP_DMA => Some(RingType::Sdma),
            _ => None,
        }
    }
    pub fn ip(self) -> u32 {
        match self {
            RingType::Gfx => AMDGPU_HW_IP_GFX,
            RingType::Compute => AMDGPU_HW_IP_COMPUTE,
            RingType::Sdma => AMDGPU_HW_IP_DMA,
        }
    }
    pub fn index(self) -> usize {
        match self {
            RingType::Gfx => 0,
            RingType::Compute => 1,
            RingType::Sdma => 2,
        }
    }
    pub fn q(self) -> Q {
        match self {
            RingType::Gfx => Q::Gfx,
            RingType::Compute => Q::Compute,
            RingType::Sdma => Q::Sdma,
        }
    }
    /// What one IB takes of the ring at most (`emit_ib_size`; SDMA's padding to its boundary).
    pub fn ib_dw(self) -> u64 {
        match self {
            RingType::Gfx => 4,
            RingType::Compute => 7,
            RingType::Sdma => 6 + 7,
        }
    }
    /// Linux's names, as a program's dump would show them.
    pub fn name(self) -> &'static str {
        match self {
            RingType::Gfx => "gfx_0.0.0",
            RingType::Compute => "comp_1.0.0",
            RingType::Sdma => "sdma0",
        }
    }
}

/// The IBs a ring can run; anything else is the program's error (Linux has BUG_ON or masks).
pub fn check_ibs(ty: RingType, ibs: &[JobIb]) -> Result<(), Errno> {
    let ok = |b: &JobIb| match ty {
        RingType::Sdma => b.va >> 48 == 0 && b.len_dw <= spkt::IB_MAX_DW,
        _ => b.va >> 48 == 0 && b.va & 3 == 0 && b.len_dw <= pm4::IB_SIZE_MASK,
    };
    if ibs.iter().all(ok) { Ok(()) } else { Err(EINVAL) }
}

pub struct Frame<'a> {
    pub ty: RingType,
    pub addrs: &'a VmAddrs,
    pub ibs: &'a [JobIb],
    pub vmid: u8,
    /// (page-table base, PASID) when the job must flush.
    pub flush: Option<(u64, u32)>,
    /// (the ring's fence slot, the last number it emitted) when the job waits for the ring's earlier jobs.
    pub sync: Option<(u64, u64)>,
    /// Another context ran last on this ring (`need_ctx_switch`).
    pub ctx_switch: bool,
    /// (the ring's fence slot, this job's number).
    pub fence: (u64, u64),
    /// gfx_v11_0.c:6267: `total_cu * 32 - 1`.
    pub max_wave_id: u32,
    /// GX-7n: the fence raises an interrupt (INT_SEL 2, SDMA's TRAP): only with the IH ring running.
    pub int: bool,
}

/// gfx_v11_0.c:5342: the raw register number, as Linux emits it.
const GDS_COMPUTE_MAX_WAVE_ID: u32 = 0x20e8;
/// nvd.h's GCR_CNTL bits of `gfx_v11_0_emit_mem_sync`: GL2 WB+INV, GLM WB+INV, GL1, GLV, GLK and GLI invalidate.
const PM4_MEM_SYNC_GCR: u32 = 1 << 15 | 1 << 14 | 1 << 5 | 1 << 4 | 1 << 9 | 1 << 8 | 1 << 7 | 1;
/// navi10_sdma_pkt_open.h's bits of `sdma_v6_0_ring_emit_mem_sync`: the same without GLM's write-back.
const SDMA_MEM_SYNC_GCR: u32 = 1 << 15 | 1 << 14 | 1 << 5 | 1 << 9 | 1 << 8 | 1 << 7 | 1;
/// The poll interval of `emit_pipeline_sync` on both kinds of ring.
const SYNC_INTERVAL: u32 = 4;
/// The most the parts other than the head and the IBs can take, the alignment's padding apart.
pub const FIXED_DW: u64 = 3 + 2 + 2 + 8;
/// The most a head can take: a mem sync, a pipeline sync, a flush with its PASID entry.
pub const HEAD_DW: u64 = 8 + 7 + 24;

impl Frame<'_> {
    fn sdma(&self) -> bool {
        self.ty == RingType::Sdma
    }

    /// Everything up to CONTEXT_CONTROL: position-independent words, built before the ring is touched.
    pub fn head(&self) -> Result<Vec<u32>, VmError> {
        let bad = |_| VmError::Ring(RingError::State);
        let mut v = Vec::new();
        if self.ibs.first().is_some_and(|b| b.flags & AMDGPU_IB_FLAG_EMIT_MEM_SYNC != 0) {
            if self.sdma() {
                v.extend([spkt::header(spkt::op::GCR_REQ, 0), 0, SDMA_MEM_SYNC_GCR << 16, SDMA_MEM_SYNC_GCR >> 16, 0]);
            } else {
                v.extend([pm4::packet3(pm4::op::ACQUIRE_MEM, 6), 0, 0xffff_ffff, 0x00ff_ffff, 0, 0, 0xa, PM4_MEM_SYNC_GCR]);
            }
        }
        if let Some((slot, seq)) = self.sync {
            if self.sdma() {
                let p = spkt::PollRegMem { hdp_flush: false, mem: true, function: 3, addr_lo: slot as u32 & !3, addr_hi: (slot >> 32) as u32, reference: seq as u32, mask: u32::MAX, interval: SYNC_INTERVAL as u16, retry: 0xfff };
                spkt::poll_regmem(&mut v, &p).map_err(bad)?;
            } else {
                let w = WaitRegMem { function: 3, mem: true, operation: 0, engine: u8::from(self.ty == RingType::Gfx), addr0: slot as u32, addr1: (slot >> 32) as u32, reference: seq as u32, mask: u32::MAX, interval: SYNC_INTERVAL };
                pm4::wait_reg_mem(&mut v, &w).map_err(bad)?;
            }
        }
        if let Some((pd, pasid)) = self.flush {
            let eng = self.ty.q().engine();
            if self.sdma() {
                vmring::sdma_flush(&mut v, self.addrs, self.vmid, pd, eng, pasid)?;
            } else {
                vmring::pm4_flush(&mut v, self.addrs, self.vmid, pd, eng, self.ty == RingType::Gfx, pasid, AckWait::Linux)?;
            }
        }
        Ok(v)
    }

    /// What the frame can take of the ring, before its padding.
    pub fn bound(&self, head: &[u32]) -> u64 {
        head.len() as u64 + FIXED_DW + self.ty.ib_dw() * self.ibs.len() as u64
    }

    /// The frame into the open submission. Encoding errors cannot happen after `check_ibs`.
    pub fn emit(&self, head: &[u32], g: &mut Ring) -> Result<(), RingError> {
        g.write(head)?;
        if self.ty == RingType::Gfx {
            g.write(&[pm4::packet3(pm4::op::CONTEXT_CONTROL, 1), cntxcntl(self.ctx_switch), 0])?;
            g.write(&frame_cntl(true))?;
        }
        for b in self.ibs {
            match self.ty {
                RingType::Gfx => g.emit_ib(self.vmid, sign_extend(b.va), b.len_dw, 0, false)?,
                RingType::Compute => {
                    if b.flags & AMDGPU_IB_FLAG_RESET_GDS_MAX_WAVE_ID != 0 {
                        g.write(&[pm4::packet3(pm4::op::SET_CONFIG_REG, 1), GDS_COMPUTE_MAX_WAVE_ID, self.max_wave_id])?;
                    }
                    g.emit(|v| pm4::indirect_buffer(v, sign_extend(b.va), b.len_dw, self.vmid, false, pm4::IB_VALID))?;
                }
                // sdma_v6_0.c:267: "base must be 32 byte aligned".
                RingType::Sdma => g.emit_ib(self.vmid, sign_extend(b.va) & !31, b.len_dw, 0, false)?,
            }
        }
        if self.ty == RingType::Gfx {
            g.write(&frame_cntl(false))?;
        }
        g.emit_fence(self.fence.0, self.fence.1, true, self.int)
    }
}

/// gfx_v11_0_ring_emit_cntxcntl: load_enable, and on a context switch the loads Linux sets.
fn cntxcntl(switch: bool) -> u32 {
    0x8000_0000 | if switch { 0x8001 | 0x0100_0000 | 0x1_0002 } else { 0 }
}

fn frame_cntl(start: bool) -> [u32; 2] {
    [pm4::packet3(pm4::op::FRAME_CONTROL, 0), u32::from(!start) << 28]
}
