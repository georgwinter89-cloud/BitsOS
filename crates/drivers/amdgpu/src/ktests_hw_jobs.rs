//! GX-8b: the job half of the hardware backend with GR-2's interpreters as the engines: the bring-up's fake chain with its
//! kernel queues, the real job thread, real `Submission`s. Fake engines only: Safe and rerunnable also while the real
//! backend is up; `gx.hw.job_program` runs the same static program the chip gets, so it needs QEMU's fake GPU node alone.

use crate::gpucore::fakechip;
use crate::hw::frame::RingType;
use crate::hw::jobs::JobCfg;
use crate::hw::vm::HwVm;
use crate::hw::{HardwareGpu, check};
use crate::ipmod::gmc11::Hub;
use crate::ktests_hw::Rig;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::pm4;
use amdgpu_fmt::sdma;
use core::sync::atomic::{AtomicBool, Ordering};
use drm::dma_buf::Exporter;
use drm::fence::{self, Fence};
use drm::gpu::{BoPlace, GpuDevice, GpuVm, PtOp};
use drm::job::{Entity, Job, JobIb, ResetSnaps, SchedPriority, Submission, UserFence};
use drm::uapi::*;
use ktest::kassert;
use linux::ab::errno_more::ECANCELED;
use linux::errno::{EINVAL, ENODEV, ETIME};
use mm::dma_pages::PinOwner;

const M: u64 = 1 << 20;
const P: u64 = 4096;
const RW: u32 = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE;
const RWX: u32 = RW | AMDGPU_VM_PAGE_EXECUTABLE;
const CODE: u64 = 1 << 34;
pub(crate) const DATA: u64 = CODE + (2 << 20);
const VRAM: u64 = CODE + (4 << 20);
/// Unmapped, next to the data's table: an invalid PTE.
pub(crate) const HOLE: u64 = DATA + (1 << 20);
const WAIT_NS: u64 = 20_000_000_000;

/// A moment for the job thread; host tests have none, so they do its round here.
pub(crate) fn give_way(g: &HardwareGpu, ns: u64) {
    #[cfg(test)]
    {
        let _ = ns;
        g.kick();
    }
    #[cfg(not(test))]
    {
        let _ = g;
        sched::sleep_ns(ns);
    }
}

pub(crate) fn clock() -> u64 {
    crate::hw::clock_ns()
}

pub(crate) fn wait(g: &HardwareGpu, f: &Arc<Fence>, ns: u64) -> bool {
    let t0 = clock();
    while !fence::is_signaled(f) {
        if clock().saturating_sub(t0) > ns {
            return false;
        }
        give_way(g, 50_000);
    }
    true
}

/// A context: what its entities share.
pub(crate) struct Cx {
    pub(crate) guilty: Arc<AtomicBool>,
    pub(crate) snaps: Arc<ResetSnaps>,
}

impl Cx {
    pub(crate) fn new(g: &HardwareGpu) -> Cx {
        Cx { guilty: Arc::new(AtomicBool::new(false)), snaps: Arc::new(ResetSnaps::new(g.reset_counter())) }
    }
    pub(crate) fn entity(&self, ty: RingType) -> Arc<Entity> {
        Entity::new(ty.ip(), 0, SchedPriority::Normal, self.guilty.clone(), self.snaps.clone())
    }
    pub(crate) fn is_guilty(&self) -> bool {
        self.guilty.load(Ordering::Acquire)
    }
}

/// A test's memory: a code object, a GTT object, a VRAM object, mapped in one VM.
pub(crate) struct Mem {
    pub(crate) vm: Arc<HwVm>,
    code: Arc<dyn Exporter>,
    gtt: Arc<dyn Exporter>,
    vram: Arc<dyn Exporter>,
    next: core::cell::Cell<u64>,
}

impl Mem {
    pub(crate) fn new(g: &Arc<HardwareGpu>) -> Result<Mem, String> {
        let e = |w: &'static str| move |x: i64| format!("{w}: errno {x}");
        let owner = PinOwner::new(50);
        let code = g.bo_memory(BoPlace::Gtt, 64, 1, false, &owner).map_err(e("code"))?;
        let gtt = g.bo_memory(BoPlace::Gtt, 4, 1, false, &owner).map_err(e("data"))?;
        let vram = g.bo_memory(BoPlace::Vram, 4, 1, false, &owner).map_err(e("VRAM"))?;
        let vm = g.new_vm(1).map_err(e("VM"))?;
        let m = |va, mem: &Arc<dyn Exporter>, place, flags| PtOp::Map { va, pages: mem.size() / P, mem: mem.clone(), first: 0, place: Some(place), uswc: false, flags };
        let ops = [m(CODE, &code, BoPlace::Gtt, RWX), m(DATA, &gtt, BoPlace::Gtt, RW), m(VRAM, &vram, BoPlace::Vram, RW)];
        if let Some(x) = vm.update(&ops).error {
            return Err(format!("map: errno {x}"));
        }
        Ok(Mem { vm, code, gtt, vram, next: core::cell::Cell::new(0) })
    }

    /// `words` in the code object, on a 32-byte boundary: the IB.
    pub(crate) fn ib(&self, words: &[u32], flags: u32) -> JobIb {
        let at = self.next.get();
        let len = (words.len() as u64 * 4).next_multiple_of(32);
        self.next.set(if at + len >= self.code.size() { 0 } else { at + len });
        let at = if at + len >= self.code.size() { 0 } else { at };
        let b: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        self.code.write(at, &b);
        JobIb { va: CODE + at, len_dw: words.len() as u32, flags }
    }

    /// GX-9a: the GTT object, for a test that plays the engine's other side.
    pub(crate) fn gtt_handle(&self) -> Arc<dyn Exporter> {
        self.gtt.clone()
    }

    pub(crate) fn gtt_u32(&self, off: u64) -> u32 {
        let mut b = [0u8; 4];
        self.gtt.read(off, &mut b);
        u32::from_le_bytes(b)
    }
}

/// An IB writing `value` to `va`.
pub(crate) fn write_ib(ty: RingType, va: u64, value: u32) -> Vec<u32> {
    let mut v = Vec::new();
    let _ = match ty {
        RingType::Sdma => sdma::write_linear(&mut v, va, &[value]),
        _ => pm4::write_data(&mut v, pm4::dst_sel::MEM, va, pm4::WR_CONFIRM, &[value]),
    };
    if ty == RingType::Sdma {
        v.resize(v.len().next_multiple_of(8), 0);
    }
    v
}

/// An IB that never ends on a PM4 ring: it waits for memory that never reads so.
pub(crate) fn stuck_ib(va: u64) -> Vec<u32> {
    let mut v = Vec::new();
    let w = pm4::WaitRegMem { function: 3, mem: true, operation: 0, engine: 0, addr0: va as u32, addr1: (va >> 32) as u32, reference: 0x1234_5678, mask: u32::MAX, interval: 4 };
    let _ = pm4::wait_reg_mem(&mut v, &w);
    v
}

/// The fences of a job the test watches.
pub(crate) struct Ends {
    pub(crate) scheduled: Arc<Fence>,
    pub(crate) finished: Arc<Fence>,
}

pub(crate) fn job(e: Arc<Entity>, ibs: Vec<JobIb>) -> (Job, Ends) {
    let c = fence::context_alloc(2);
    let scheduled = Fence::new(c, 1, "amdgpu", "test");
    let finished = Fence::new(c + 1, 1, "amdgpu", "test");
    (Job { entity: e, ibs, shadow: None, scheduled: scheduled.clone(), finished: finished.clone() }, Ends { scheduled, finished })
}

pub(crate) fn sub(g: &HardwareGpu, m: &Mem, jobs: Vec<Job>, deps: Vec<Arc<Fence>>, user_fence: Option<UserFence>) -> Submission {
    Submission { jobs, vm: m.vm.clone(), deps, user_fence, bos: Vec::new(), generation: g.vram_lost_counter() }
}

/// One job, submitted, awaited; its status.
pub(crate) fn run_one(g: &HardwareGpu, m: &Mem, cx: &Cx, ty: RingType, words: &[u32]) -> Result<i32, String> {
    let (j, e) = job(cx.entity(ty), vec![m.ib(words, 0)]);
    g.submit(sub(g, m, vec![j], Vec::new(), None)).map_err(|x| format!("{}: submit: errno {x}", ty.name()))?;
    g.kick();
    kassert_end(g, &e, ty)?;
    Ok(fence::status(&e.finished))
}

pub(crate) fn kassert_end(g: &HardwareGpu, e: &Ends, ty: RingType) -> Result<(), String> {
    if !wait(g, &e.finished, WAIT_NS) {
        return Err(format!("{}: no end in {} s", ty.name(), WAIT_NS / 1_000_000_000));
    }
    if !fence::is_signaled(&e.scheduled) {
        return Err(format!("{}: finished before scheduled", ty.name()));
    }
    Ok(())
}

pub(crate) fn rig(cfg: JobCfg) -> Result<Rig, String> {
    let d = fakechip::discovery_780m();
    let r = Rig::chain(&d, 512 * M)?;
    r.start_jobs(cfg)?;
    Ok(r)
}

/// Where each queue's ring lies.
fn ring_mc(r: &Rig, ty: RingType) -> u64 {
    match ty {
        RingType::Gfx => r.vmi.gfx.ring,
        RingType::Compute => r.vmi.mes.compute_ring,
        RingType::Sdma => r.vmi.sdma.map_or(0, |s| s.layout.ring),
    }
}

/// How often the ring holds the packet header `h` with first body dword `w` (None: any).
fn count(words: &[u32], h: u32, w: Option<u32>) -> usize {
    words.windows(2).filter(|x| x[0] == h && w.is_none_or(|v| v == x[1])).count()
}

/// The proof the tool runs before the node, on the fake chain: a job per ring, from VRAM, into GTT and VRAM.
pub(crate) fn proof() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let lines = check::jobs(&r.gpu)?;
    kassert!(lines.len() == 6, "{lines:?}");
    kassert!(r.gpu.vms_alive() == 0, "a VM stayed");
    let j = r.gpu.jobs().ok_or("no jobs")?;
    kassert!(j.shared.jobs_done.load(Ordering::Acquire) == 6, "{:?}", j.describe());
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_proof", Safe, proof);

/// Many jobs on every ring: rings wrap, the queue holds what does not fit, each job's write lands, in order.
pub(crate) fn many() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    for (n, ty) in RingType::ALL.into_iter().enumerate() {
        let e = cx.entity(ty);
        let mut last = None;
        for k in 0..300u32 {
            let (j, ends) = job(e.clone(), vec![m.ib(&write_ib(ty, DATA + n as u64 * 4, k + 1), 0)]);
            g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("submit {k}: errno {x}"))?;
            g.kick();
            last = Some(ends);
        }
        let last = last.ok_or("no job")?;
        kassert_end(g, &last, ty)?;
        kassert!(fence::status(&last.finished) == 1, "{}: status {}", ty.name(), fence::status(&last.finished));
        kassert!(m.gtt_u32(n as u64 * 4) == 300, "{}: reads {}", ty.name(), m.gtt_u32(n as u64 * 4));
    }
    kassert!(m.vm.last_fault().is_none(), "{:?}", m.vm.last_fault());
    let j = g.jobs().ok_or("no jobs")?;
    kassert!(j.shared.jobs_done.load(Ordering::Acquire) >= 900 && !j.shared.lost.load(Ordering::Acquire), "{:?}", j.describe());
    // One VM, one VMID: only its first job flushes on each ring.
    let flushes = r.fake(|f| f.hubs.requests.iter().filter(|(h, e, _)| *h == Hub::Gfx && [0, 1, 12].contains(e)).count());
    kassert!(flushes >= 1 && flushes <= 3, "{flushes} flushes for one VM");
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_many", Safe, many);

/// The user fence is the leader's, 64 bits, before its finished fence; a gang's members all run.
pub(crate) fn user_fence() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let (j1, e1) = job(cx.entity(RingType::Compute), vec![m.ib(&write_ib(RingType::Compute, DATA + 0x10, 0xaa), 0)]);
    let (j2, e2) = job(cx.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA + 0x14, 0xbb), 0)]);
    let uf = UserFence { mem: m.gtt.clone(), offset: 0x100, value: 0x1122_3344_5566_7788 };
    g.submit(sub(g, &m, vec![j1, j2], Vec::new(), Some(uf))).map_err(|x| format!("submit: errno {x}"))?;
    g.kick();
    kassert_end(g, &e1, RingType::Compute)?;
    kassert_end(g, &e2, RingType::Gfx)?;
    let mut b = [0u8; 8];
    m.gtt.read(0x100, &mut b);
    kassert!(u64::from_le_bytes(b) == 0x1122_3344_5566_7788, "user fence {b:x?}");
    kassert!(m.gtt_u32(0x10) == 0xaa && m.gtt_u32(0x14) == 0xbb, "gang {:#x} {:#x}", m.gtt_u32(0x10), m.gtt_u32(0x14));
    // A leader that does not run writes none.
    let bad = Cx::new(g);
    bad.guilty.store(true, Ordering::Release);
    let (j, e) = job(bad.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA + 0x18, 0xcc), 0)]);
    let uf = UserFence { mem: m.gtt.clone(), offset: 0x108, value: 7 };
    g.submit(sub(g, &m, vec![j], Vec::new(), Some(uf))).map_err(|x| format!("submit: errno {x}"))?;
    g.kick();
    kassert_end(g, &e, RingType::Gfx)?;
    m.gtt.read(0x108, &mut b);
    kassert!(u64::from_le_bytes(b) == 0 && fence::status(&e.finished) == -(ECANCELED as i32), "{b:x?} status {}", fence::status(&e.finished));
    kassert!(m.gtt_u32(0x18) == 0, "the job of a guilty context ran");
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_user_fence", Safe, user_fence);

/// A dependency on another fence holds the job; one on a job of its own ring puts a pipeline sync in its frame instead.
pub(crate) fn deps() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    // An outside fence.
    let c = fence::context_alloc(1);
    let outside = Fence::new(c, 1, "test", "outside");
    let (j, e) = job(cx.entity(RingType::Sdma), vec![m.ib(&write_ib(RingType::Sdma, DATA + 0x20, 5), 0)]);
    g.submit(sub(g, &m, vec![j], vec![outside.clone()], None)).map_err(|x| format!("submit: errno {x}"))?;
    g.kick();
    give_way(g, 5_000_000);
    kassert!(!fence::is_signaled(&e.scheduled) && m.gtt_u32(0x20) == 0, "the job did not wait for its dependency");
    outside.signal();
    kassert_end(g, &e, RingType::Sdma)?;
    kassert!(m.gtt_u32(0x20) == 5, "reads {}", m.gtt_u32(0x20));
    // The same ring: both placed in one round (the ring's slot address is what the sync names).
    let jobs = g.jobs().ok_or("no jobs")?;
    let ty = RingType::Compute;
    let slot = crate::hw::rings::fence_slot(&r.vmi, ty);
    let e_ = cx.entity(ty);
    let (a, ea) = job(e_.clone(), vec![m.ib(&write_ib(ty, DATA + 0x24, 6), 0)]);
    let (b, eb) = job(e_, vec![m.ib(&write_ib(ty, DATA + 0x28, 7), 0)]);
    let dep = ea.finished.clone();
    {
        let _held = jobs.no_round();
        g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("submit: errno {x}"))?;
        g.submit(sub(g, &m, vec![b], vec![dep], None)).map_err(|x| format!("submit: errno {x}"))?;
    }
    g.kick();
    kassert_end(g, &eb, ty)?;
    let words = r.ring_words(ring_mc(&r, ty));
    let waits = words.windows(3).filter(|x| x[0] == pm4::packet3(pm4::op::WAIT_REG_MEM, 5) && x[2] == slot as u32).count();
    kassert!(waits >= 1, "no pipeline sync on the fence slot {slot:#x} in the compute ring");
    kassert!(m.gtt_u32(0x24) == 6 && m.gtt_u32(0x28) == 7, "{} {}", m.gtt_u32(0x24), m.gtt_u32(0x28));
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_deps", Safe, deps);

/// The frame's parts Linux emits appear on each ring; a higher priority goes first among what is ready.
pub(crate) fn frames() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let ty = RingType::Gfx;
    let once = cx.entity(ty);
    let (j, e) = job(once.clone(), vec![m.ib(&write_ib(ty, DATA, 1), 0)]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &e, ty)?;
    let w = r.ring_words(ring_mc(&r, ty));
    kassert!(count(&w, pm4::packet3(pm4::op::CONTEXT_CONTROL, 1), Some(0x8101_8003)) == 1, "no CONTEXT_CONTROL with the context switch's loads");
    kassert!(count(&w, pm4::packet3(pm4::op::FRAME_CONTROL, 0), Some(0)) == 1 && count(&w, pm4::packet3(pm4::op::FRAME_CONTROL, 0), Some(1 << 28)) == 1, "no FRAME_CONTROL pair");
    kassert!(count(&w, pm4::packet3(pm4::op::PFP_SYNC_ME, 0), Some(0)) >= 1, "no PFP_SYNC_ME after the flush");
    kassert!(count(&w, pm4::packet3(pm4::op::RELEASE_MEM, 6), Some(pm4::rm::GFX11_FENCE)) >= 1, "no RELEASE_MEM with Linux's cache bits");
    // The same entity again: no switch (`current_ctx` is its fence context).
    let (j, e) = job(once, vec![m.ib(&write_ib(ty, DATA, 2), 0)]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &e, ty)?;
    let w = r.ring_words(ring_mc(&r, ty));
    kassert!(count(&w, pm4::packet3(pm4::op::CONTEXT_CONTROL, 1), Some(0x8000_0000)) == 1, "a job of the same context got a context switch");
    // Priority: a high job queued after a normal one goes first (both ready at one pick; each writes its turn).
    let jobs = g.jobs().ok_or("no jobs")?;
    let other = Cx::new(g);
    let normal = other.entity(RingType::Sdma);
    let high = Entity::new(RingType::Sdma.ip(), 0, SchedPriority::High, other.guilty.clone(), other.snaps.clone());
    let (a, ea) = job(normal, vec![m.ib(&write_ib(RingType::Sdma, DATA + 0x40, 0xa), 0)]);
    let (b, eb) = job(high, vec![m.ib(&write_ib(RingType::Sdma, DATA + 0x40, 0xb), 0)]);
    {
        let _held = jobs.no_round();
        g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("{x}"))?;
        g.submit(sub(g, &m, vec![b], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    }
    g.kick();
    kassert_end(g, &ea, RingType::Sdma)?;
    kassert_end(g, &eb, RingType::Sdma)?;
    kassert!(m.gtt_u32(0x40) == 0xa, "the high-priority job did not go first: {:#x} is the last write", m.gtt_u32(0x40));
    // VRAM: an engine's write, the CPU's window on it.
    kassert!(run_one(g, &m, &cx, RingType::Sdma, &write_ib(RingType::Sdma, VRAM + 0x10, 0xabcd))? == 1, "the VRAM job failed");
    let mut b = [0u8; 4];
    kassert!(m.vram.read(0x10, &mut b) && u32::from_le_bytes(b) == 0xabcd, "VRAM reads {b:x?}");
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_frames", Safe, frames);

/// Two rings sharing a VMID: the flush of the other ring has not signaled yet, so this ring flushes too (Linux's rule); a
/// third job on the first ring needs none. Two VMs: the VMID's table follows.
pub(crate) fn flush_rule() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let (a, ea) = job(cx.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA, 1), 0)]);
    let (b, eb) = job(cx.entity(RingType::Sdma), vec![m.ib(&write_ib(RingType::Sdma, DATA + 4, 2), 0)]);
    {
        let _held = jobs.no_round();
        g.submit(sub(g, &m, vec![a], Vec::new(), None)).map_err(|x| format!("{x}"))?;
        g.submit(sub(g, &m, vec![b], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    }
    g.kick();
    kassert_end(g, &ea, RingType::Gfx)?;
    kassert_end(g, &eb, RingType::Sdma)?;
    let reqs = |e: u8| r.fake(|f| f.hubs.requests.iter().filter(|(h, x, _)| *h == Hub::Gfx && *x == e).count());
    let before = (reqs(RingType::Gfx.q().engine()), reqs(RingType::Sdma.q().engine()));
    kassert!(before.0 >= 1 && before.1 >= 1, "flushes per engine: {before:?}: the second ring must flush too");
    kassert!(run_one(g, &m, &cx, RingType::Gfx, &write_ib(RingType::Gfx, DATA + 8, 3))? == 1, "job");
    let after = reqs(RingType::Gfx.q().engine());
    kassert!(after == before.0, "a job with nothing to flush flushed: {after} after {}", before.0);
    // An update moves the VM's sequence: the next job flushes.
    let m2 = g.bo_memory(BoPlace::Gtt, 1, 1, false, &PinOwner::new(50)).map_err(|x| format!("{x}"))?;
    let op = PtOp::Map { va: DATA + (8 << 20), pages: 1, mem: m2, first: 0, place: Some(BoPlace::Gtt), uswc: false, flags: RW };
    kassert!(m.vm.update(core::slice::from_ref(&op)).error.is_none(), "map");
    kassert!(run_one(g, &m, &cx, RingType::Gfx, &write_ib(RingType::Gfx, DATA + (8 << 20), 4))? == 1, "job");
    kassert!(reqs(RingType::Gfx.q().engine()) == after + 1, "a job after an update did not flush");
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_flush_rule", Safe, flush_rule);

/// More VMs than VMIDs on the rings at once: each job waits for a VMID in the queue, none is lost, each VM sees its own memory.
pub(crate) fn vmids() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let mems: Vec<Mem> = (0..12).map(|_| Mem::new(g)).collect::<Result<_, _>>()?;
    let cx = Cx::new(g);
    let e = cx.entity(RingType::Sdma);
    let mut ends = Vec::new();
    for (n, m) in mems.iter().enumerate() {
        for k in 0..8u32 {
            let (j, en) = job(e.clone(), vec![m.ib(&write_ib(RingType::Sdma, DATA, n as u32 * 100 + k), 0)]);
            g.submit(sub(g, m, vec![j], Vec::new(), None)).map_err(|x| format!("{x}"))?;
            ends.push(en);
        }
        g.kick();
    }
    for en in &ends {
        kassert_end(g, en, RingType::Sdma)?;
    }
    for (n, m) in mems.iter().enumerate() {
        kassert!(m.gtt_u32(0) == n as u32 * 100 + 7, "VM {n}: reads {}", m.gtt_u32(0));
    }
    drop(mems);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_vmids", Safe, vmids);

/// What cannot run: TMZ and unaligned IBs answer EINVAL at the CS, a stale generation ends unrun, no job half ENODEV.
pub(crate) fn refused() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let ty = RingType::Gfx;
    let (j, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, DATA, 1), AMDGPU_IB_FLAGS_SECURE)]);
    kassert!(g.submit(sub(g, &m, vec![j], Vec::new(), None)) == Err(EINVAL), "TMZ on GFX went through");
    kassert!(fence::status(&e.finished) == -(EINVAL as i32), "a refused job must end: {}", fence::status(&e.finished));
    let (j, _) = job(cx.entity(ty), vec![JobIb { va: CODE + 2, len_dw: 4, flags: 0 }]);
    kassert!(g.submit(sub(g, &m, vec![j], Vec::new(), None)) == Err(EINVAL), "an unaligned IB went through");
    // SDMA masks an IB's low bits as Linux does: it runs.
    let w = write_ib(RingType::Sdma, DATA + 8, 2);
    let ib = m.ib(&w, 0);
    let (j, e) = job(cx.entity(RingType::Sdma), vec![JobIb { va: ib.va + 4, ..ib }]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &e, RingType::Sdma)?;
    kassert!(m.gtt_u32(8) == 2, "reads {}", m.gtt_u32(8));
    // A stale generation.
    let (j, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, DATA + 12, 3), 0)]);
    let mut s = sub(g, &m, vec![j], Vec::new(), None);
    s.generation = g.vram_lost_counter().wrapping_add(1);
    g.submit(s).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &e, ty)?;
    kassert!(fence::status(&e.finished) == -(ECANCELED as i32) && m.gtt_u32(12) == 0, "generation: {} reads {}", fence::status(&e.finished), m.gtt_u32(12));
    drop(m);
    let g2 = r.gpu.clone();
    let done = r.finish_jobs();
    let m = Mem::new(&g2)?;
    let cx = Cx::new(&g2);
    let (j, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, DATA, 1), 0)]);
    kassert!(g2.submit(sub(&g2, &m, vec![j], Vec::new(), None)) == Err(ENODEV) && fence::status(&e.finished) == -(ENODEV as i32), "a device with no job half took a job");
    done
}
ktest::ktest!("gx.hw.job_refused", Safe, refused);

/// A fault on GFX or SDMA: the engine drops the access and its job goes on to its fence (Linux: no error), the fault is the
/// VM's alone, the next job runs. Nobody is guilty.
pub(crate) fn fault_goes_on() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let a = Mem::new(g)?;
    let b = Mem::new(g)?;
    for ty in [RingType::Gfx, RingType::Sdma] {
        let cx = Cx::new(g);
        kassert!(run_one(g, &a, &cx, ty, &write_ib(ty, HOLE, 0xdead))? == 1, "{}: the job failed", ty.name());
        let t0 = clock();
        while a.vm.last_fault().is_none() && clock().saturating_sub(t0) < 1_000_000_000 {
            // The fault shows at the next poll; a job keeps the poll going.
            let _ = run_one(g, &b, &cx, ty, &write_ib(ty, DATA, 1));
            give_way(g, 100_000);
        }
        let f = a.vm.last_fault().ok_or_else(|| format!("{}: no fault reported", ty.name()))?;
        kassert!(f.addr == HOLE & !0xfff && f.vmhub == AMDGPU_VMHUB_TYPE_GFX, "{}: {f:?}", ty.name());
        kassert!(!cx.is_guilty() && g.reset_counter() == 0, "{}: blamed for a dropped access", ty.name());
        kassert!(run_one(g, &a, &cx, ty, &write_ib(ty, DATA + 0x60, 9))? == 1 && a.gtt_u32(0x60) == 9, "{}: the next job", ty.name());
        a.gtt.write(0x60, &[0; 4]);
    }
    kassert!(b.vm.last_fault().is_none(), "the other VM got the fault");
    let j = g.jobs().ok_or("no jobs")?;
    kassert!(!j.shared.lost.load(Ordering::Acquire), "{:?}", j.describe());
    drop((a, b));
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_fault_goes_on", Safe, fault_goes_on);

/// A fault on the compute ring: CPC stops there. The queue comes back through MES at once; that job ends ETIME, its context
/// guilty; a job of another context behind it ECANCELED, and then the device's reset counter moves; the ring runs on.
pub(crate) fn fault_stops_job() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let ty = RingType::Compute;
    let a = Mem::new(g)?;
    let b = Mem::new(g)?;
    let cx = Cx::new(g);
    let other = Cx::new(g);
    kassert!(run_one(g, &a, &cx, ty, &write_ib(ty, HOLE, 0xdead))? == -(ETIME as i32), "the stopped job must end ETIME");
    kassert!(cx.is_guilty(), "its context is not guilty");
    kassert!(a.vm.last_fault().is_some_and(|f| f.addr == HOLE & !0xfff), "{:?}", a.vm.last_fault());
    kassert!(cx.snaps.reset_counter.load(Ordering::Acquire) != g.reset_counter() && g.reset_counter() == 0, "blame did not move the context off the counter ({}); the device's must stay", g.reset_counter());
    kassert!(run_one(g, &b, &other, ty, &write_ib(ty, DATA, 7))? == 1 && b.gtt_u32(0) == 7, "the ring does not run on");
    kassert!(b.vm.last_fault().is_none() && !other.is_guilty(), "the other context paid");
    // Now with a job of another context behind it on the ring.
    let cy = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let (j1, e1) = job(cy.entity(ty), vec![a.ib(&write_ib(ty, HOLE + 8, 0xdead), 0)]);
    let (j2, e2) = job(other.entity(ty), vec![b.ib(&write_ib(ty, DATA + 4, 8), 0)]);
    {
        let _held = jobs.no_round();
        g.submit(sub(g, &a, vec![j1], Vec::new(), None)).map_err(|x| format!("{x}"))?;
        g.submit(sub(g, &b, vec![j2], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    }
    g.kick();
    kassert_end(g, &e1, ty)?;
    kassert_end(g, &e2, ty)?;
    kassert!(fence::status(&e1.finished) == -(ETIME as i32) && fence::status(&e2.finished) == -(ECANCELED as i32), "fates {} {}", fence::status(&e1.finished), fence::status(&e2.finished));
    kassert!(g.reset_counter() == 1, "another context lost a job: the counter says {}", g.reset_counter());
    kassert!(!other.is_guilty() && cy.is_guilty(), "guilt");
    kassert!(run_one(g, &b, &other, ty, &write_ib(ty, DATA + 8, 9))? == 1 && b.gtt_u32(8) == 9, "the ring does not run on after the second fault");
    // The other queues never noticed.
    kassert!(run_one(g, &b, &other, RingType::Gfx, &write_ib(RingType::Gfx, DATA + 12, 10))? == 1, "GFX");
    let j = g.jobs().ok_or("no jobs")?;
    kassert!(!j.shared.lost.load(Ordering::Acquire) && j.shared.stalls.load(Ordering::Acquire) == 2, "{:?}", j.describe());
    drop((a, b));
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_fault_stops_job", Safe, fault_stops_job);

/// A hang with no rung to take it (GX-9a's `ktests_hw_ladder` has the rungs): no progress for the ring's timeout. The decoded
/// dump goes out, the job ends ETIME, its context guilty, the counter moves, every other job ECANCELED, the device lost: submit
/// ENODEV. Nothing goes back while the engines may run it.
fn hang_on(ty: RingType) -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([30_000, 30_000, 30_000]), ladder_from: crate::hw::jobs::Rung::None, ..JobCfg::default() };
    let r = rig(cfg)?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let other = Cx::new(g);
    let (j, bad) = job(cx.entity(ty), vec![m.ib(&stuck_ib(DATA + 0x80), 0)]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    let (j, behind) = job(other.entity(ty), vec![m.ib(&write_ib(ty, DATA + 0x84, 1), 0)]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("{x}"))?;
    g.kick();
    kassert_end(g, &bad, ty)?;
    kassert_end(g, &behind, ty)?;
    kassert!(fence::status(&bad.finished) == -(ETIME as i32), "hung job: {}", fence::status(&bad.finished));
    kassert!(fence::status(&behind.finished) == -(ECANCELED as i32), "job behind: {}", fence::status(&behind.finished));
    kassert!(cx.is_guilty() && !other.is_guilty(), "guilt {} {}", cx.is_guilty(), other.is_guilty());
    kassert!(g.reset_counter() >= 1, "the counter did not move");
    let jobs = g.jobs().ok_or("no jobs")?;
    let t0 = clock();
    while !jobs.shared.lost.load(Ordering::Acquire) && clock().saturating_sub(t0) < 1_000_000_000 {
        give_way(g, 100_000);
    }
    kassert!(jobs.shared.lost.load(Ordering::Acquire), "the device is not lost");
    let text = jobs.describe().join("\n");
    kassert!(text.contains("the last hang") && text.contains("timeout") && text.contains("WAIT_REG_MEM") && text.contains("LOST") && text.contains("display writes while the backend ran: 0"), "{text}");
    let (j, e) = job(other.entity(ty), vec![m.ib(&write_ib(ty, DATA + 0x88, 2), 0)]);
    kassert!(g.submit(sub(g, &m, vec![j], Vec::new(), None)) == Err(ENODEV) && fence::status(&e.finished) == -(ENODEV as i32), "a lost device took a job");
    // What the engines may still run stays held until they are halted.
    let vm_refs = Arc::strong_count(&m.vm);
    kassert!(vm_refs > 1, "the VM of a job an engine may still run went");
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.len() >= 2, "held {}", held.len());
    drop(held);
    drop(m);
    r.finish()
}

pub(crate) fn hang_gfx() -> ktest::TestResult {
    hang_on(RingType::Gfx)
}
pub(crate) fn hang_compute() -> ktest::TestResult {
    hang_on(RingType::Compute)
}
ktest::ktest!("gx.hw.job_hang_gfx", Safe, hang_gfx);
ktest::ktest!("gx.hw.job_hang_compute", Safe, hang_compute);

/// An idle GPU costs no spinning: a look a second. And `stop`: what waits ends ECANCELED, the thread goes, it stays gone.
pub(crate) fn idle_and_stop() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    kassert!(run_one(g, &m, &cx, RingType::Sdma, &write_ib(RingType::Sdma, DATA, 1))? == 1, "job");
    #[cfg(not(test))]
    {
        let jobs = g.jobs().ok_or("no jobs")?;
        sched::sleep_ns(20_000_000);
        let r0 = jobs.shared.rounds.load(Ordering::Acquire);
        sched::sleep_ns(100_000_000);
        let spins = jobs.shared.rounds.load(Ordering::Acquire) - r0;
        kassert!(spins <= 2, "{spins} rounds in 100 ms with nothing to do");
    }
    let c = fence::context_alloc(1);
    let never = Fence::new(c, 1, "test", "never");
    let (j, e) = job(cx.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA + 4, 2), 0)]);
    g.submit(sub(g, &m, vec![j], vec![never], None)).map_err(|x| format!("{x}"))?;
    g.kick();
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.is_empty(), "nothing ran, but {} are held", held.len());
    kassert!(fence::status(&e.finished) == -(ECANCELED as i32), "a queued job at the stop: {}", fence::status(&e.finished));
    kassert!(g.stop_jobs().is_none(), "a second stop found a job half");
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.job_stop", Safe, idle_and_stop);

/// A static program on the backend's node, with GR-2's interpreters as the engines, against its expected text.
#[cfg(not(test))]
fn on_node(cfg: JobCfg, prog: &str, args: &[&str], expected: &str) -> ktest::TestResult {
    kassert!(vfs::root_ns().lookup(prog).is_ok(), "{prog} is not in this image");
    let r = rig(cfg)?;
    let dev = drm::add_device(r.gpu.clone()).map_err(|e| format!("add: errno {e}"))?;
    let lowest = drm::device::devices().iter().map(|d| d.render_minor()).min();
    let run = (|| -> ktest::TestResult {
        kassert!(lowest == Some(dev.render_minor()), "another node would answer the program: {lowest:?}, ours {}", dev.render_minor());
        let out = proc::stdio::Capture::new(1 << 16);
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let mut o = proc::SpawnOptions::new(args, &env, policy::ProgramId(1));
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with(prog, &o).map_err(|e| format!("spawn {prog}: {e}"))?;
        let ended = p.wait(Some(120_000_000_000)).is_some();
        if !ended {
            p.kill(9);
            p.wait(Some(5_000_000_000));
        }
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        kassert!(ended && p.status() == Some(proc::ExitStatus::Exited(0)), "{prog} ended {:?}: {text}", p.status());
        let diff: Vec<(&str, &str)> = text.lines().zip(expected.lines()).filter(|(a, b)| a != b).collect();
        kassert!(diff.is_empty() && text.lines().count() == expected.lines().count(), "{prog} differs: {diff:?}");
        Ok(())
    })();
    drm::remove_device(&dev);
    run?;
    // A lost device's jobs keep their VMs until the engines are halted (here: until the job half is gone).
    drop(r.gpu.stop_jobs());
    kassert!(r.gpu.vms_alive() == 0, "a VM stayed after the program");
    r.finish()
}

/// The node's whole path through ioctls: the same static program the chip's check list runs (GEM create, VA map, SDMA, GFX
/// and compute IBs, WAIT_CS, CPU read-back, two fds, a fault on one).
#[cfg(not(test))]
fn program() -> ktest::TestResult {
    on_node(JobCfg::default(), "/bin/abi-drm-jobs", &["abi-drm-jobs", "run"], include_str!("../../../../host/tests/gx/fixtures/abi-drm-jobs.txt"))
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.job_program", QemuOnly, program);

/// The same program's last word: a hang as a program meets it (a timeout shorter than the chip's 10 s). The queue rung takes
/// it: the job ETIME, the context told, the device goes on.
#[cfg(not(test))]
fn program_hang() -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([100_000, 100_000, 100_000]), ..JobCfg::default() };
    on_node(cfg, "/bin/abi-drm-jobs", &["abi-drm-jobs", "hang"], include_str!("../../../../host/tests/gx/fixtures/abi-drm-jobs-hang.txt"))
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.job_program_hang", QemuOnly, program_hang);

/// GX-9a: the same program when no rung takes it: the device lost, and the program's next CS says so.
#[cfg(not(test))]
fn program_lost() -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([100_000, 100_000, 100_000]), ladder_from: crate::hw::jobs::Rung::None, ..JobCfg::default() };
    on_node(cfg, "/bin/abi-drm-jobs", &["abi-drm-jobs", "hang"], include_str!("../../../../host/tests/gx/fixtures/abi-drm-jobs-lost.txt"))
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.job_program_lost", QemuOnly, program_lost);

/// GX-9a: the same program through GC's re-init.
#[cfg(not(test))]
fn program_reinit() -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([100_000, 100_000, 100_000]), ladder_from: crate::hw::jobs::Rung::Gc, ..JobCfg::default() };
    on_node(cfg, "/bin/abi-drm-jobs", &["abi-drm-jobs", "hang"], include_str!("../../../../host/tests/gx/fixtures/abi-drm-jobs-hang.txt"))
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.job_program_reinit", QemuOnly, program_reinit);

/// CS's checks with the job half behind them, against what CachyOS answered (no job ever exists in this one).
#[cfg(not(test))]
fn abi_cs() -> ktest::TestResult {
    on_node(JobCfg::default(), "/bin/abi-drm-cs", &["abi-drm-cs"], include_str!("../../../../host/tests/dr/fixtures/abi-drm-cs.txt"))
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.job_abi_cs", QemuOnly, abi_cs);
