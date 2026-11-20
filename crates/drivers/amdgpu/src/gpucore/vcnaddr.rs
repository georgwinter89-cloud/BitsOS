//! GX-8v: VCN 0's addresses (the discovery table's UVD segment 1, NBIO's doorbell side) and the stage's buffer layout
//! (`gpucore::vcn` re-exports them).

use super::disc::Discovery;
use super::hwid;
use crate::ipmod::vcn4::{self, R};
use crate::ipmod::{ih6, nbio7, sdma6};
use alloc::vec::Vec;

/// The BO's layout (carve-out offsets from its start): Linux's vcpu_bo (stack, context, shared page), the SRAM
/// list, the ring with its metadata, the writeback slots.
pub mod lay {
    use amdgpu_fmt::vcn::{CONTEXT_BYTES, STACK_BYTES};
    pub const STACK: u64 = 0;
    pub const CONTEXT: u64 = STACK_BYTES;
    pub const SHARED: u64 = CONTEXT + CONTEXT_BYTES;
    pub const SRAM: u64 = SHARED + 0x1000;
    pub const RING: u64 = SRAM + 0x1000;
    pub const WB: u64 = RING + 0x2000;
    pub const END: u64 = WB + 0x1000;
}
/// The BO the stage asks for (the live allocator rounds it up to a whole 2 MiB piece).
pub const BO_BYTES: u64 = 0x10_0000;
/// Writeback slots inside `lay::WB` (bytes): the write pointer's shadow, the job path's fence.
pub const WB_WPTR: u64 = 0;
pub const WB_JOB_FENCE: u64 = 0x80;

/// NBIO's doorbell side for VCN 0 as BAR5 bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VcnNbio {
    pub aper_en: u32,
    pub rsmu_index: u32,
    pub rsmu_data: u32,
    /// The RSMU index value of GDC0_BIF_VCN0_DOORBELL_RANGE (its byte address in NBIO's segment 3).
    pub range: u32,
}

/// VCN 0's registers as BAR5 bytes (the discovery table's UVD segment 1), NBIO's doorbell side.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VcnAddrs {
    base: u32,
    pub nbio: Option<VcnNbio>,
    /// IH_VMID_0_LUT_MM's byte (OSSSYS): the VCN ring names a job's PASID there.
    pub lut_mm: Option<u32>,
}

impl VcnAddrs {
    pub fn from_discovery(d: &Discovery) -> Option<VcnAddrs> {
        let base = d.reg(hwid::VCN, 0, 1, 0)?;
        let nb = |at: (usize, u32)| d.reg(hwid::NBIF, 0, at.0, at.1).and_then(|x| x.checked_mul(4));
        let nbio = (|| Some(VcnNbio { aper_en: nb(nbio7::RCC_DOORBELL_APER_EN)?, rsmu_index: nb(sdma6::RSMU_INDEX)?, rsmu_data: nb(sdma6::RSMU_DATA)?, range: nb(vcn4::VCN0_DOORBELL_RANGE)? }))();
        let lut_mm = d.reg(hwid::OSSSYS, 0, ih6::IH_VMID_0_LUT_MM.0, ih6::IH_VMID_0_LUT_MM.1).and_then(|x| x.checked_mul(4));
        Some(VcnAddrs { base, nbio, lut_mm })
    }

    pub fn byte(&self, r: R) -> Option<u32> {
        self.base.checked_add(r.at().0)?.checked_mul(4)
    }

    pub fn name_at(&self, byte: u32) -> Option<&'static str> {
        R::ALL.into_iter().find(|&r| self.byte(r) == Some(byte)).map(R::name)
    }

    /// Every register the stage reads (VCN's, NBIO's aperture and the RSMU pair).
    pub fn readable(&self) -> Vec<u32> {
        let mut v: Vec<u32> = R::ALL.into_iter().filter_map(|r| self.byte(r)).collect();
        if let Some(n) = self.nbio {
            v.extend([n.aper_en, n.rsmu_index, n.rsmu_data]);
        }
        v
    }

    /// Every register the stage writes: the CPU's part of the DPG start and stop, RSMU's data (the index only with
    /// `rsmu_value`: the live owner checks it).
    pub fn writable(&self) -> Vec<u32> {
        let mut v: Vec<u32> = vcn4::CPU_WRITES.into_iter().filter_map(|r| self.byte(r)).collect();
        if let Some(n) = self.nbio {
            v.push(n.rsmu_data);
        }
        v
    }

    /// The only value the RSMU index may take for this stage.
    pub fn rsmu_value(&self) -> Option<u32> {
        self.nbio.map(|n| n.range)
    }
}

/// The stage's buffer (kept for the boot: the VCPU reads its context, the shared page and the ring).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VcnBo {
    pub handle: u32,
    pub offset: u64,
    pub size: u64,
}

/// Where everything lies (MC addresses, VMID 0, MMHUB's system aperture).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    pub bo_mc: u64,
    pub shared: u64,
    pub sram: u64,
    pub ring: u64,
    pub wptr: u64,
    pub job_fence: u64,
}

impl Layout {
    pub fn new(bo_mc: u64) -> Layout {
        let wb = bo_mc + lay::WB;
        Layout {
            bo_mc,
            shared: bo_mc + lay::SHARED,
            sram: bo_mc + lay::SRAM,
            ring: bo_mc + lay::RING,
            wptr: wb + WB_WPTR,
            job_fence: wb + WB_JOB_FENCE,
        }
    }
}

