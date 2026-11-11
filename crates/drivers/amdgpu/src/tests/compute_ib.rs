//! The bug of 39918f01 on the fake chip: MEC drops a job whose IB lies above the hole unless the address is sign-extended
//! (CP_HQD_ERROR.SUA_ERROR, no fence). Measured on the 780M on 4.11.; until then the fake took any address.

use crate::gpucore::mesfake::HQD_ERROR_SUA;
use crate::hw::frame::RingType;
use crate::hw::jobs::JobCfg;
use crate::ipmod::mes11::M;
use crate::ktests_hw_jobs::{Cx, DATA, Mem, job, kassert_end, rig, sub, write_ib};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::pm4;
use drm::dma_buf::Exporter;
use drm::fence;
use drm::gpu::{BoPlace, GpuDevice, GpuVm, PtOp};
use drm::job::JobIb;
use drm::uapi::{AMDGPU_VM_PAGE_EXECUTABLE, AMDGPU_VM_PAGE_READABLE};
use mm::dma_pages::PinOwner;

/// Where RADV keeps its IBs: above the hole, the VA as user space gives it (48 bits).
const HIGH: u64 = 0x8001_0000_0000;

/// A code object mapped at [`HIGH`], holding `words`.
fn code_up_high(g: &Arc<crate::hw::HardwareGpu>, m: &Mem, words: &[u32]) -> Arc<dyn Exporter> {
    let code = g.bo_memory(BoPlace::Gtt, 1, 1, false, &PinOwner::new(50)).unwrap();
    let flags = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE;
    let map = PtOp::Map { va: HIGH, pages: 1, mem: code.clone(), first: 0, place: Some(BoPlace::Gtt), uswc: false, flags };
    assert!(m.vm.update(&[map]).error.is_none());
    let b: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    code.write(0, &b);
    code
}

/// What the kernel's frame does with RADV's address: the job runs. Without `sign_extend` in `hw/frame.rs` it drops here.
#[test]
fn a_job_above_the_hole_runs_on_the_compute_queue() {
    let r = rig(JobCfg::default()).unwrap();
    let g = &r.gpu;
    let m = Mem::new(g).unwrap();
    let words = write_ib(RingType::Compute, DATA, 0x5a5a);
    let _code = code_up_high(g, &m, &words);
    let ib = JobIb { va: HIGH, len_dw: words.len() as u32, flags: 0 };
    let (j, e) = job(Cx::new(g).entity(RingType::Compute), vec![ib]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).unwrap();
    g.kick();
    kassert_end(g, &e, RingType::Compute).unwrap();
    assert_eq!(fence::status(&e.finished), 1, "no fence: the fake dropped the job");
    assert_eq!(m.gtt_u32(0), 0x5a5a);
    assert_eq!(r.fake(|f| f.mes.compute_hqd(M::CpHqdError)) & HQD_ERROR_SUA, 0);
    drop(m);
    r.finish_jobs().unwrap();
}

/// The fake's side: an IB packet with the upper half not sign-extended, here from inside a job's IB because the frame
/// cannot make one any more. No write, no fence, SUA_ERROR, as measured.
#[test]
fn the_fake_drops_a_job_whose_ib_address_is_not_sign_extended() {
    let r = rig(JobCfg { timeouts_us: Some([30_000, 30_000, 30_000]), ..JobCfg::default() }).unwrap();
    let g = &r.gpu;
    let m = Mem::new(g).unwrap();
    let inner = write_ib(RingType::Compute, DATA, 0x5a5a);
    let _code = code_up_high(g, &m, &inner);
    let call = [pm4::packet3(pm4::op::INDIRECT_BUFFER, 2), HIGH as u32, (HIGH >> 32) as u32, inner.len() as u32 | pm4::IB_VALID];
    let (j, e) = job(Cx::new(g).entity(RingType::Compute), vec![m.ib(&call, 0)]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).unwrap();
    g.kick();
    kassert_end(g, &e, RingType::Compute).unwrap();
    assert!(fence::status(&e.finished) < 0, "the job ended well: {}", fence::status(&e.finished));
    assert_eq!(m.gtt_u32(0), 0, "the IB ran");
    assert_eq!(r.fake(|f| f.mes.compute_hqd(M::CpHqdError)) & HQD_ERROR_SUA, HQD_ERROR_SUA);
    drop(g.stop_jobs());
    drop(m);
    r.finish().unwrap();
}
