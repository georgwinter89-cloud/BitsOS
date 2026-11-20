//! GX-8v in-kernel tests of VCN's job half on the fake chain: the backend's chain (until=backend), the VCN stage with
//! MMHUB's contexts 1-7, the job half (`hw::vcn`, offered at once: no probe), then a program's decode session through
//! `GpuDevice::submit` (what the CS ioctl calls) with its fence; INFO's answers (HW_IP_INFO for the unified ring and 0
//! rings for VCN_DEC and JPEG, VIDEO_CAPS packed as Linux 7.3.1 reports VCN 4.0.2); the CPU's MMHUB writes (the
//! contexts and engine 0's range, no display field); Linux's refusals (a user fence, a secure IB, a gang); a hang
//! (ETIME and guilty, the others and the queued ones ECANCELED, the ring lost, VCN powered down).

use crate::gpucore::fakechip;
use crate::gpucore::gmc::HubAddrs;
use crate::gpucore::psp::Until;
use crate::gpucore::pspfake::Hostile;
use crate::gpucore::ucode::ListOpts;
use crate::gpucore::vcn::{self, VcnAddrs};
use crate::gpucore::vcnvm;
use crate::hw::HardwareGpu;
use crate::hw::jobs::JobCfg;
use crate::hw::vcn::VcnUp;
use crate::ipmod::gmc11::Hub;
use crate::ktests_bringup::{GOP, one_variant};
use crate::ktests_hw::Rig;
use crate::ktests_hw_jobs::{Cx, job, wait};
use crate::ktests_vcn::inputs;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::vcn as pkt;
use drm::dma_buf::Exporter;
use drm::fence;
use drm::gpu::{BoPlace, GpuDevice, GpuVm, PtOp};
use drm::job::{JobIb, Submission, UserFence};
use drm::uapi::*;
use ktest::kassert;
use mm::dma_pages::PinOwner;

const M: u64 = 1 << 20;
const AT: u64 = 1 << 36;

/// The backend with VCN's half on the fake chain, as the boot leaves it.
pub(crate) fn rig() -> Result<Rig, String> {
    let d = fakechip::discovery_780m();
    let opts = ListOpts::PSP13_0_4;
    let (mut out, mut owner, mut bo, _) = one_variant(&d, 512 * M, fakechip::StartAs::Cold, Until::Backend, true, Hostile::None, &opts, &[], GOP, crate::gpucore::gfxring::Variant::DEFAULT, &|_| {})?;
    kassert!(out.passed(), "the chain: {:?}", out.error);
    let mut psp = out.kept.take().ok_or("no kept PSP")?;
    let mm = vcn::mm_display_bytes(&d);
    let inp = inputs(&d, owner.hw().hw(), &out.report, &mm)?;
    let r = vcn::enable(&mut owner, &mut bo, &mut psp, &inp);
    // Per VMID CNTL and PAGE_TABLE_END differ from the firmware's (START is 0 already), and engine 0's range.
    kassert!(r.passed() && r.contexts.len() == 7 * 3 + 2, "the VCN stage: {:?}, contexts {}", r.error, r.contexts.len());
    let rig = Rig::on_chain(&d, 512 * M, &out, owner, bo)?;
    rig.reset.keep_psp(Some(psp));
    rig.start_jobs(JobCfg::default())?;
    let l = r.layout.ok_or("no layout")?;
    let up = VcnUp {
        addrs: VcnAddrs::from_discovery(&d).ok_or("addrs")?,
        layout: l,
        start: r.wptr,
        doorbell: r.doorbell,
        mm: HubAddrs::from_discovery(&d, Hub::Mm).ok_or("MMHUB")?,
        smu: crate::gpucore::pspbringup::GfxInputs::from_discovery(&d).ok_or("smu")?.smu,
        timeout_us: amdgpu_fmt::watch::TimeoutClass::Video.linux_us(),
    };
    rig.gpu.start_vcn(rig.fake_chain(), up)?;
    Ok(rig)
}

fn bytes(w: &[u32]) -> Vec<u8> {
    w.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// A program's memory: IBs and messages in one VRAM object at `AT`, mapped in its VM.
struct Prog {
    vm: Arc<crate::hw::vm::HwVm>,
    mem: Arc<dyn Exporter>,
}

impl Prog {
    fn new(g: &Arc<HardwareGpu>) -> Result<Prog, String> {
        let e = |w: &'static str| move |x: i64| format!("{w}: errno {x}");
        let mem = g.bo_memory(BoPlace::Vram, 8, 1, false, &PinOwner::new(50)).map_err(e("memory"))?;
        let vm = g.new_vm(7).map_err(e("VM"))?;
        let flags = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE | AMDGPU_VM_PAGE_EXECUTABLE;
        if let Some(x) = vm.update(&[PtOp::Map { va: AT, pages: 8, mem: mem.clone(), first: 0, place: Some(BoPlace::Vram), uswc: false, flags }]).error {
            return Err(format!("map: errno {x}"));
        }
        Ok(Prog { vm, mem })
    }
    /// A decode IB at page `n` naming a message at page `n + 4`.
    fn dec(&self, n: u64, msg: &[u32]) -> JobIb {
        let ib = pkt::dec_sw_ib(AT + (n + 4) * 4096);
        self.mem.write(n * 4096, &bytes(&ib));
        self.mem.write((n + 4) * 4096, &bytes(msg));
        JobIb { va: AT + n * 4096, len_dw: ib.len() as u32, flags: 0 }
    }
    fn sub(&self, g: &HardwareGpu, jobs: Vec<drm::job::Job>, uf: Option<UserFence>) -> Submission {
        Submission { jobs, vm: self.vm.clone(), deps: Vec::new(), user_fence: uf, bos: Vec::new(), generation: g.vram_lost_counter() }
    }
}

/// soc21.c's vcn_4_0_0 tables for VCN 0 (codec index, width, height, level), as INFO packs them.
const DECODE: [(usize, u32, u32, u32); 5] = [(3, 4096, 4096, 52), (4, 8192, 4352, 186), (5, 16384, 16384, 0), (6, 8192, 4352, 0), (7, 8192, 4352, 0)];
const ENCODE: [(usize, u32, u32, u32); 3] = [(3, 4096, 4096, 0), (4, 8192, 4352, 0), (7, 8192, 4352, 0)];

/// INFO's answers through drm's own code (`drm::info`, what the INFO ioctl calls).
fn check_info(g: &HardwareGpu) -> ktest::TestResult {
    kassert!(g.rings_ready(AMDGPU_HW_IP_VCN_ENC) == 1 && g.rings_ready(AMDGPU_HW_IP_VCN_DEC) == 0 && g.rings_ready(AMDGPU_HW_IP_VCN_JPEG) == 0, "rings");
    let ip = |t: u32| drm::info::hw_ip_info(g, &drm_amdgpu_info__query_hw_ip { r#type: t, ip_instance: 0 }).map_err(|e| format!("hw_ip_info {t}: {e}"));
    let h = ip(AMDGPU_HW_IP_VCN_ENC)?;
    kassert!(h.available_rings == 1 && h.hw_ip_version_major == 4 && h.ip_discovery_version == 0x0004_0002 && h.ib_start_alignment == 256, "HW_IP_INFO {h:?}");
    // VCN 4 has no decode-only ring (Mesa decodes on the unified one); JPEG's ring is not built.
    for t in [AMDGPU_HW_IP_VCN_DEC, AMDGPU_HW_IP_VCN_JPEG, AMDGPU_HW_IP_UVD, AMDGPU_HW_IP_VCE, AMDGPU_HW_IP_UVD_ENC] {
        let h = ip(t)?;
        kassert!(h.available_rings == 0, "HW_IP_INFO {t}: rings {:#x}", h.available_rings);
    }
    // VIDEO_CAPS, whatever the ring: every slot as Linux fills it (valid, the size, its pixels, the level).
    for (ty, table) in [(AMDGPU_INFO_VIDEO_CAPS_DECODE, &DECODE[..]), (AMDGPU_INFO_VIDEO_CAPS_ENCODE, &ENCODE[..])] {
        let caps = drm::info::video_caps(g, ty).map_err(|e| format!("VIDEO_CAPS {ty}: {e}"))?;
        for (i, c) in caps.codec_info.iter().enumerate() {
            let want = table.iter().find(|t| t.0 == i).map_or((0, 0, 0, 0, 0), |&(_, w, h, l)| (1, w, h, w * h, l));
            kassert!((c.valid, c.max_width, c.max_height, c.max_pixels_per_frame, c.max_level) == want, "VIDEO_CAPS {ty} codec {i}: {c:?}");
        }
    }
    kassert!(drm::info::video_caps(g, 2) == Err(linux::errno::EINVAL), "VIDEO_CAPS of an unknown type");
    Ok(())
}

/// The CPU's MMHUB writes of the whole bring-up: the contexts and engine 0's range (`vcnvm::writable`, at least one
/// each), else only the GMC stage's own flush engines; never a display field.
fn check_mm_writes(rig: &Rig) -> ktest::TestResult {
    let d = fakechip::discovery_780m();
    let mm = HubAddrs::from_discovery(&d, Hub::Mm).ok_or("MMHUB")?;
    let ours = vcnvm::writable(&mm);
    let (writes, guard) = rig.fake(|f| (f.writes.iter().copied().filter(|&(b, _)| f.hubs.owns(b) == Some(Hub::Mm)).collect::<Vec<_>>(), f.guard.clone()));
    for &(b, v) in &writes {
        let class = guard.class(b);
        kassert!(class.is_none_or(|c| !c.is_display()) && (ours.contains(&b) || class.is_some()), "a CPU MMHUB write {b:#x} <- {v:#x} ({class:?})");
    }
    let range = [crate::ipmod::gmc11::R::RangeLo(vcnvm::ENGINE), crate::ipmod::gmc11::R::RangeHi(vcnvm::ENGINE)].map(|r| mm.byte(r));
    kassert!(range.iter().all(|r| r.is_some_and(|r| writes.iter().any(|w| w.0 == r))), "engine 0's range never written: {writes:x?}");
    kassert!(writes.iter().filter(|w| ours.contains(&w.0)).count() == 7 * 3 + 2, "the contexts' writes: {writes:x?}");
    Ok(())
}

pub(crate) fn check_jobs() -> ktest::TestResult {
    let rig = rig()?;
    let g = rig.gpu.clone();
    check_info(&g)?;
    check_mm_writes(&rig)?;
    // A program's decode session: create, one decode, destroy, as one job of three IBs.
    let p = Prog::new(&g)?;
    let h = 0x77;
    let mut decode = pkt::dec_create_msg(h);
    decode[3] = 1;
    let ibs = alloc::vec![p.dec(0, &pkt::dec_create_msg(h)), p.dec(1, &decode), p.dec(2, &pkt::dec_destroy_msg(h))];
    let cx = Cx::new(&g);
    let (j, e) = job(drm::job::Entity::new(AMDGPU_HW_IP_VCN_ENC, 0, drm::job::SchedPriority::Normal, cx.guilty.clone(), cx.snaps.clone()), ibs);
    let decodes0 = rig.fake(|f| f.vcn.decodes);
    g.submit(p.sub(&g, alloc::vec![j], None)).map_err(|x| format!("submit: errno {x}"))?;
    g.kick();
    kassert!(wait(&g, &e.finished, 10_000_000_000) && fence::is_signaled(&e.scheduled) && fence::status(&e.finished) == 1, "the job: status {}", fence::status(&e.finished));
    let (decodes, ibs, errs, writes, luts) = rig.fake(|f| (f.vcn.decodes, f.vcn.ibs.clone(), f.vcn.errors.clone(), f.vcn.ring_writes.clone(), f.hubs.lut_mm_writes.clone()));
    kassert!(decodes == decodes0 + 1 && errs.is_empty(), "the VCPU: {decodes} decodes, errors {errs:?}");
    kassert!(ibs.iter().rev().take(3).all(|i| vcnvm::VMIDS.contains(&i.vmid) && i.checksum_ok), "the job's IBs ran at a program's VMID: {:?}", ibs.iter().rev().take(3).collect::<Vec<_>>());
    // The job's PASID reached IH_VMID_n_LUT_MM of its VMID.
    let vmid = ibs.last().map_or(0, |i| i.vmid);
    kassert!(luts.last() == Some(&(vmid, p.vm.pasid())) && !luts.is_empty(), "LUT_MM writes {luts:x?}, VMID {vmid}, PASID {:#x}", p.vm.pasid());
    // GX-D9: the ring wrote only MMHUB's context bases, engine 0 and LUT_MM; the CPU no display field.
    let d = fakechip::discovery_780m();
    let mm = HubAddrs::from_discovery(&d, Hub::Mm).ok_or("MMHUB")?;
    let allowed = vcnvm::ring_writable(&mm, VcnAddrs::from_discovery(&d).and_then(|a| a.lut_mm));
    kassert!(!writes.is_empty() && writes.iter().all(|(r, _)| allowed.regs.contains(r)), "ring writes {writes:x?}");
    kassert!(rig.fake(|f| f.display_writes.is_empty() && f.scanout_writes.is_empty()), "display writes");
    // Linux's refusals: a user fence (no_user_fence), a secure IB, a gang of two.
    let ib = p.dec(3, &pkt::dec_create_msg(1));
    let uf = UserFence { mem: p.mem.clone(), offset: 7 * 4096, value: 1 };
    let (j, _) = job(drm::job::Entity::new(AMDGPU_HW_IP_VCN_ENC, 0, drm::job::SchedPriority::Normal, cx.guilty.clone(), cx.snaps.clone()), alloc::vec![ib]);
    kassert!(g.submit(p.sub(&g, alloc::vec![j], Some(uf))) == Err(linux::errno::EINVAL), "user fence");
    let (j, e) = job(drm::job::Entity::new(AMDGPU_HW_IP_VCN_ENC, 0, drm::job::SchedPriority::Normal, cx.guilty.clone(), cx.snaps.clone()), alloc::vec![JobIb { flags: AMDGPU_IB_FLAGS_SECURE, ..ib }]);
    kassert!(g.submit(p.sub(&g, alloc::vec![j], None)) == Err(linux::errno::EINVAL) && fence::status(&e.finished) < 0, "secure IB");
    let mk = || job(drm::job::Entity::new(AMDGPU_HW_IP_VCN_ENC, 0, drm::job::SchedPriority::Normal, cx.guilty.clone(), cx.snaps.clone()), alloc::vec![ib]).0;
    kassert!(g.submit(p.sub(&g, alloc::vec![mk(), mk()], None)) == Err(linux::errno::EINVAL), "gang");
    kassert!(!cx.is_guilty(), "guilty");
    g.stop_jobs();
    Ok(())
}

/// A job VCN never finishes (its fence never comes): after the video timeout it ends with ETIME and its context is
/// guilty, the job behind it on the ring ECANCELED, a job still waiting for a dependency ECANCELED as well (it would
/// never run); VCN is powered down, the ring lost for the boot, the display untouched.
pub(crate) fn check_hang() -> ktest::TestResult {
    let rig = rig()?;
    let g = rig.gpu.clone();
    rig.fake(|f| f.vcn.hostile.push(crate::gpucore::vcnfake::VcnHostile::NoFence));
    let p = Prog::new(&g)?;
    let (ca, cb) = (Cx::new(&g), Cx::new(&g));
    let ent = |c: &Cx| drm::job::Entity::new(AMDGPU_HW_IP_VCN_ENC, 0, drm::job::SchedPriority::Normal, c.guilty.clone(), c.snaps.clone());
    let (ja, a) = job(ent(&ca), alloc::vec![p.dec(0, &pkt::dec_create_msg(0x51))]);
    let (jc, c) = job(ent(&cb), alloc::vec![p.dec(1, &pkt::dec_create_msg(0x52))]);
    let (jb, b) = job(ent(&cb), alloc::vec![p.dec(2, &pkt::dec_create_msg(0x53))]);
    let never = drm::fence::Fence::new(fence::context_alloc(1), 1, "amdgpu", "never");
    g.submit(p.sub(&g, alloc::vec![ja], None)).map_err(|x| format!("submit A: errno {x}"))?;
    g.submit(p.sub(&g, alloc::vec![jc], None)).map_err(|x| format!("submit C: errno {x}"))?;
    let mut sb = p.sub(&g, alloc::vec![jb], None);
    sb.deps.push(never.clone());
    g.submit(sb).map_err(|x| format!("submit B: errno {x}"))?;
    g.kick();
    let ended = wait(&g, &a.finished, 10_000_000_000) && wait(&g, &c.finished, 1_000_000_000) && wait(&g, &b.finished, 2_000_000_000);
    let st = |e: &crate::ktests_hw_jobs::Ends| fence::status(&e.finished);
    let (etime, canceled) = (-(linux::errno::ETIME as i32), -(linux::ab::errno_more::ECANCELED as i32));
    kassert!(ended && st(&a) == etime && st(&c) == canceled && st(&b) == canceled, "ends: A {}, C {}, B {}", st(&a), st(&c), st(&b));
    kassert!([&a, &b, &c].iter().all(|e| fence::is_signaled(&e.scheduled)), "a scheduled fence left");
    kassert!(ca.is_guilty() && !cb.is_guilty(), "guilty: A {}, the others {}", ca.is_guilty(), cb.is_guilty());
    let v = g.vcn().ok_or("no VCN half")?;
    kassert!(v.lost.load(core::sync::atomic::Ordering::Acquire) && v.hangs.load(core::sync::atomic::Ordering::Acquire) == 1 && g.rings_ready(AMDGPU_HW_IP_VCN_ENC) == 0, "the ring is not lost");
    let (jd, _) = job(ent(&cb), alloc::vec![p.dec(3, &pkt::dec_create_msg(0x54))]);
    kassert!(g.submit(p.sub(&g, alloc::vec![jd], None)) == Err(linux::errno::ENODEV), "a job after the hang");
    kassert!(rig.fake(|f| !f.smu.vcn_on && f.display_writes.is_empty() && f.scanout_writes.is_empty()), "VCN still powered or the display touched");
    never.signal();
    g.stop_jobs();
    Ok(())
}

fn hang() -> ktest::TestResult {
    check_hang()
}
ktest::ktest!("gx.vcn.hang", Safe, hang);

fn jobs() -> ktest::TestResult {
    check_jobs()
}
ktest::ktest!("gx.vcn.jobs", Safe, jobs);
