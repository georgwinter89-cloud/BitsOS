//! GX-8v: what a program's job on VCN's ring is made of (pure, built before the ring is touched), and how an MMHUB
//! fault of such a job reaches its VM. BitsOS's fault path reads GFXHUB's status alone (`hw::lost`); VCN reaches
//! memory through MMHUB, so its half reads MMHUB's L2 protection fault status and address (reads only: no clear, an
//! MMHUB write the GX-D9 plan does not need) and gives a new fault to the VCN job of that VMID
//! (`AMDGPU_INFO_GPUVM_FAULT` then answers it with `AMDGPU_VMHUB_TYPE_MM0`). As the status is not cleared, it holds
//! the first fault and counts later ones only in MORE_FAULTS: a program sees the first MMHUB fault of the boot.

use super::vcn::VcnUp;
use super::vm::HwVm;
use crate::gpucore::gmc::FaultRead;
use crate::gpucore::vcnvm;
use crate::ipmod::gmc11::Fault;
use alloc::vec::Vec;
use amdgpu_fmt::addr::sign_extend;
use amdgpu_fmt::vcn as pkt;
use drm::gpu::VmFault;
use drm::job::{JobIb, Submission};
use drm::uapi::*;
use linux::errno::EINVAL;
use vfs::Errno;

/// The IBs this ring runs (Linux's multimedia limit; dword aligned).
pub fn check_ibs(ibs: &[JobIb]) -> Result<(), Errno> {
    let ok = |b: &JobIb| b.va >> 48 == 0 && b.va & 3 == 0 && b.len_dw != 0 && b.len_dw <= pkt::IB_MAX_DW && b.flags & AMDGPU_IB_FLAGS_SECURE == 0;
    if !ibs.is_empty() && ibs.iter().all(ok) { Ok(()) } else { Err(EINVAL) }
}

/// One job's frame: the VMID's flush, its PASID in IH_VMID_n_LUT_MM, the IBs, FENCE (the job fence's slot) + TRAP,
/// END.
pub fn frame_words(up: &VcnUp, vmid: u8, pd: u64, pasid: u32, ibs: &[JobIb], seq: u64) -> Option<Vec<u32>> {
    let mut v = vcnvm::flush(&up.mm, vmid, pd)?;
    v.extend(vcnvm::pasid_map(up.addrs.lut_mm, vmid, pasid));
    for b in ibs {
        pkt::ib(&mut v, vmid, sign_extend(b.va), b.len_dw).ok()?;
    }
    pkt::fence(&mut v, up.layout.job_fence, seq).ok()?;
    v.push(pkt::cmd::END);
    Some(v)
}

/// A status worth a look: set, readable, and not the one already seen.
pub fn new_fault(f: &FaultRead, seen: u32) -> Option<(u32, u64)> {
    f.status.filter(|&s| s != 0 && s != u32::MAX && s != seen).map(|s| (s, f.addr.unwrap_or(0)))
}

/// Gives the fault to the job of its VMID among `subs` (vmid, submission); false: no job of ours has that VMID.
pub fn give_fault<'a>(status: u32, addr: u64, mut subs: impl Iterator<Item = (u32, &'a Submission)>) -> bool {
    let vmid = Fault::decode(status).vmid;
    let Some((_, sub)) = subs.find(|(v, _)| *v == vmid) else { return false };
    if let Some(vm) = sub.vm.as_any().and_then(|a| a.downcast_ref::<HwVm>()) {
        vm.note_fault(VmFault { addr, status, vmhub: AMDGPU_VMHUB_TYPE_MM0 });
    }
    true
}
