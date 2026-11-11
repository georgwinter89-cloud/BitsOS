//! GX-8b: the job half's bad days, on the fake chain as `ktests_hw_jobs`: a frame that fails, a ladder that refuses, a fault
//! of another client, a hang with two VMs in flight, what stays held after a stop. Fake engines only: Safe and rerunnable.

use crate::hw::frame::RingType;
use crate::hw::jobs::{Held, JobCfg, Rung};
use crate::hw::HardwareGpu;
use crate::hw::live::HeldJobs;
use crate::ipmod::gmc11::Hub;
use crate::ktests_hw::Rig;
use crate::ktests_hw_jobs::{Cx, DATA, Ends, HOLE, Mem, clock, give_way, job, kassert_end, rig, stuck_ib, sub, wait, write_ib};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use drm::fence::{self, Fence};
use drm::gpu::{GpuDevice, GpuVm};
use ksync::SpinLock;
use ktest::kassert;
use linux::ab::errno_more::ECANCELED;
use linux::errno::{EIO, ENODEV, ETIME};

const HOUR_US: u64 = 3_600_000_000;
const SECOND_NS: u64 = 1_000_000_000;

fn status(e: &Ends) -> i32 {
    fence::status(&e.finished)
}

/// A job that never ends, on its ring; back once the ring has it.
fn stuck(r: &Rig, m: &Mem, cx: &Cx, ty: RingType, at: u64) -> Result<Ends, String> {
    let g = &r.gpu;
    let (j, e) = job(cx.entity(ty), vec![m.ib(&stuck_ib(DATA + at), 0)]);
    g.submit(sub(g, m, vec![j], Vec::new(), None)).map_err(|x| format!("{}: submit: errno {x}", ty.name()))?;
    g.kick();
    kassert!(wait(g, &e.scheduled, 20 * SECOND_NS), "{}: never on its ring", ty.name());
    Ok(e)
}

fn flushes(r: &Rig, ty: RingType) -> usize {
    r.fake(|f| f.hubs.requests.iter().filter(|(h, e, _)| *h == Hub::Gfx && *e == ty.q().engine()).count())
}

/// A gang whose first member gets the VMID's flush and then no frame: the member behind it flushes in its place. Its grant
/// said no flush, and without one its IB runs on stale TLB entries, translations of freed memory among them.
pub(crate) fn gang_flush() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let before = (flushes(&r, RingType::Gfx), flushes(&r, RingType::Compute));
    let (a, ea) = job(cx.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA, 1), 0)]);
    let (b, eb) = job(cx.entity(RingType::Compute), vec![m.ib(&write_ib(RingType::Compute, DATA + 4, 2), 0)]);
    {
        let _held = jobs.no_round();
        r.chain_busy.store(1, Ordering::Release);
        g.submit(sub(g, &m, vec![a, b], Vec::new(), None)).map_err(|x| format!("submit: errno {x}"))?;
    }
    g.kick();
    kassert_end(g, &ea, RingType::Gfx)?;
    kassert_end(g, &eb, RingType::Compute)?;
    kassert!(status(&ea) == -(EIO as i32) && status(&eb) == 1, "fates {} {}", status(&ea), status(&eb));
    kassert!(m.gtt_u32(0) == 0 && m.gtt_u32(4) == 2, "writes {} {}", m.gtt_u32(0), m.gtt_u32(4));
    kassert!(flushes(&r, RingType::Compute) > before.1, "the gang's second member ran with no flush");
    kassert!(flushes(&r, RingType::Gfx) == before.0, "a frame that never was flushed");
    // The VM forgot the flush that never ran: its next job on that ring does it.
    let (j, e) = job(cx.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA, 3), 0)]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("submit: errno {x}"))?;
    g.kick();
    kassert_end(g, &e, RingType::Gfx)?;
    kassert!(status(&e) == 1 && flushes(&r, RingType::Gfx) > before.0, "the next job did not flush: {}", status(&e));
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_gang_flush", Safe, gang_flush);

/// A doorbell that does not land: the frame is on the ring with its write pointer published, so it may run. The job ends EIO
/// with everything it reaches kept until the engines are halted; the gang's next member gets no words on a lost device.
pub(crate) fn doorbell_refused() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let (a, ea) = job(cx.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA, 1), 0)]);
    let (b, eb) = job(cx.entity(RingType::Compute), vec![m.ib(&write_ib(RingType::Compute, DATA + 4, 2), 0)]);
    {
        let _held = jobs.no_round();
        r.fake(|f| f.refuse_doorbells = 1);
        g.submit(sub(g, &m, vec![a, b], Vec::new(), None)).map_err(|x| format!("submit: errno {x}"))?;
    }
    g.kick();
    kassert_end(g, &ea, RingType::Gfx)?;
    kassert_end(g, &eb, RingType::Compute)?;
    kassert!(status(&ea) == -(EIO as i32) && status(&eb) == -(ECANCELED as i32), "fates {} {}", status(&ea), status(&eb));
    kassert!(m.gtt_u32(4) == 0, "the lost device got the gang's next frame");
    kassert!(jobs.shared.lost.load(Ordering::Acquire), "the device is not lost");
    drop(jobs);
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.len() == 1, "held {}", held.len());
    drop(m);
    kassert!(g.vms_alive() == 1, "the VM of a frame that may run went: {} alive", g.vms_alive());
    drop(held);
    kassert!(g.vms_alive() == 0, "{} VMs after the hold ended", g.vms_alive());
    r.finish()
}
ktest::ktest!("gx.hw.job_doorbell_refused", Safe, doorbell_refused);

/// What a job's waiter finds the moment its fence ends: the device's state must be there already, as it asks at once.
struct AtEnd {
    shared: Arc<crate::hw::jobs::Shared>,
    seen: SpinLock<Option<(bool, u32)>>,
    _reg: SpinLock<Option<fence::Registration>>,
}

impl fence::FenceCb for AtEnd {
    fn call(&self) {
        *self.seen.lock() = Some((self.shared.lost.load(Ordering::Acquire), self.shared.reset_counter.load(Ordering::Acquire)));
    }
}

impl AtEnd {
    fn on(e: &Ends, shared: &Arc<crate::hw::jobs::Shared>) -> Arc<AtEnd> {
        let a = Arc::new(AtEnd { shared: shared.clone(), seen: SpinLock::named("gx.ktest.at_end", None), _reg: SpinLock::named("gx.ktest.at_end_reg", None) });
        *a._reg.lock() = fence::add_callback(&e.finished, a.clone());
        a
    }
    /// `Fence::signal` publishes the end before it runs the callbacks: a waiter on another CPU may be here before this one ran.
    fn check(&self, g: &HardwareGpu) -> ktest::TestResult {
        let t0 = clock();
        let seen = loop {
            let seen = *self.seen.lock();
            if seen.is_some() || clock().saturating_sub(t0) >= SECOND_NS {
                break seen;
            }
            give_way(g, 1_000_000);
        };
        let Some((lost, reset)) = seen else {
            return Err(String::from("the fence ended and its callback did not run within 1 s"));
        };
        kassert!(lost && reset > 0, "the fence ended before the device said so: lost {lost}, reset counter {reset}");
        // The registration holds this object: no cycle stays behind.
        drop(self._reg.lock().take());
        Ok(())
    }
}

/// The ladder's own end (`apply`): a job that hangs on the watch's timeout, with no rung to take it.
pub(crate) fn hang_state_first() -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([50_000; 3]), ladder_from: Rung::None, ..JobCfg::default() };
    let r = rig(cfg)?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let (j, e) = job(cx.entity(RingType::Gfx), vec![m.ib(&stuck_ib(DATA + 0x80), 0)]);
    let jobs = g.jobs().ok_or("no jobs")?;
    let seen = AtEnd::on(&e, &jobs.shared);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("submit: errno {x}"))?;
    g.kick();
    kassert_end(g, &e, RingType::Gfx)?;
    kassert!(status(&e) == -(ETIME as i32), "fate {}", status(&e));
    seen.check(g)?;
    drop(jobs);
    let held = g.stop_jobs().ok_or("no job half")?;
    drop(m);
    drop(held);
    r.finish()
}
ktest::ktest!("gx.hw.job_hang_state_first", Safe, hang_state_first);

/// GR-3's ladder does not take the ring (no hang by its count): the jobs end all the same, the device counts as lost, and it
/// says so once. Left on the ring they would report again at every round and never end.
pub(crate) fn ladder_refuses() -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([HOUR_US; 3]), ..JobCfg::default() };
    let r = rig(cfg)?;
    let g = &r.gpu;
    let (a, b) = (Mem::new(g)?, Mem::new(g)?);
    let (cx, behind, other) = (Cx::new(g), Cx::new(g), Cx::new(g));
    let head = stuck(&r, &a, &cx, RingType::Gfx, 0x80)?;
    let second = stuck(&r, &a, &behind, RingType::Gfx, 0x84)?;
    let third = stuck(&r, &b, &other, RingType::Compute, 0x88)?;
    let jobs = g.jobs().ok_or("no jobs")?;
    let seen = AtEnd::on(&head, &jobs.shared);
    jobs.no_round().hung(RingType::Gfx.index(), clock() / 1000);
    seen.check(g)?;
    kassert!(status(&head) == -(ETIME as i32), "head {}", status(&head));
    kassert!(status(&second) == -(ECANCELED as i32) && status(&third) == -(ECANCELED as i32), "others {} {}", status(&second), status(&third));
    kassert!(cx.is_guilty() && !behind.is_guilty() && !other.is_guilty(), "guilt");
    kassert!(g.reset_counter() == 1 && jobs.shared.lost.load(Ordering::Acquire), "counter {}, {:?}", g.reset_counter(), jobs.describe());
    for _ in 0..5 {
        give_way(g, 1_000_000);
    }
    kassert!(jobs.shared.hangs.load(Ordering::Acquire) == 1, "reported {} times", jobs.shared.hangs.load(Ordering::Acquire));
    let (j, e) = job(behind.entity(RingType::Gfx), vec![a.ib(&write_ib(RingType::Gfx, DATA, 1), 0)]);
    kassert!(g.submit(sub(g, &a, vec![j], Vec::new(), None)) == Err(ENODEV) && status(&e) == -(ENODEV as i32), "a lost device took a job");
    drop(jobs);
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.len() == 3, "held {}", held.len());
    drop(held);
    drop((a, b));
    r.finish()
}
ktest::ktest!("gx.hw.job_ladder_refuses", Safe, ladder_refuses);

/// A fault in the compute job's VMID from a client that is not CPC (another ring's job, a shader): no stop of the queue. The
/// job meets the timeout as any other: the queue rung, which counts no rebuild after a fault.
pub(crate) fn other_client_fault() -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([HOUR_US, 100_000, HOUR_US]), ..JobCfg::default() };
    let r = rig(cfg)?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let jobs = g.jobs().ok_or("no jobs")?;
    let e = stuck(&r, &m, &cx, RingType::Compute, 0x80)?;
    let vmid = jobs.no_round().head_vmid(RingType::Compute).ok_or("nothing on the ring")?;
    r.fake(|f| f.hubs.fault_vm(Hub::Gfx, vmid, HOLE, true, 3, 1 << 4));
    kassert_end(g, &e, RingType::Compute)?;
    kassert!(status(&e) == -(ETIME as i32) && cx.is_guilty(), "{}", status(&e));
    kassert!(m.vm.last_fault().is_some_and(|f| f.addr == HOLE & !0xfff), "{:?}", m.vm.last_fault());
    let t0 = clock();
    while jobs.shared.recovered[Rung::Queue as usize].load(Ordering::Acquire) == 0 && clock().saturating_sub(t0) < SECOND_NS {
        give_way(g, 100_000);
    }
    kassert!(jobs.shared.stalls.load(Ordering::Acquire) == 0, "the queue was rebuilt as if its client had stopped");
    kassert!(jobs.shared.recovered[Rung::Queue as usize].load(Ordering::Acquire) == 1 && !jobs.shared.lost.load(Ordering::Acquire), "{:?}", jobs.describe());
    drop(jobs);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.job_other_client_fault", Safe, other_client_fault);

/// A hang that ends the device while another fd has a job in flight on another ring: the hung job ETIME and only its context
/// guilty; the other fd's job ECANCELED, and that fd learns of the reset through the counter. Both keep what their jobs reach.
pub(crate) fn hang_two_vms() -> ktest::TestResult {
    let cfg = JobCfg { timeouts_us: Some([100_000, HOUR_US, HOUR_US]), ladder_from: Rung::None, ..JobCfg::default() };
    let r = rig(cfg)?;
    let g = &r.gpu;
    let (a, b) = (Mem::new(g)?, Mem::new(g)?);
    let (ca, cb) = (Cx::new(g), Cx::new(g));
    let eb = stuck(&r, &b, &cb, RingType::Compute, 0x80)?;
    let ea = stuck(&r, &a, &ca, RingType::Gfx, 0x80)?;
    kassert_end(g, &ea, RingType::Gfx)?;
    kassert_end(g, &eb, RingType::Compute)?;
    kassert!(status(&ea) == -(ETIME as i32) && status(&eb) == -(ECANCELED as i32), "fates {} {}", status(&ea), status(&eb));
    kassert!(ca.is_guilty() && !cb.is_guilty(), "guilt {} {}", ca.is_guilty(), cb.is_guilty());
    kassert!(g.reset_counter() >= 1 && cb.snaps.reset_counter.load(Ordering::Acquire) != g.reset_counter(), "the other fd cannot see the reset: {}", g.reset_counter());
    kassert!(Arc::strong_count(&a.vm) > 1 && Arc::strong_count(&b.vm) > 1, "a VM of a job an engine may still run went");
    kassert!(a.vm.last_fault().is_none() && b.vm.last_fault().is_none(), "a fault out of nowhere");
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.len() == 2, "held {}", held.len());
    drop(held);
    drop((a, b));
    r.finish()
}
ktest::ktest!("gx.hw.job_hang_two_vms", Safe, hang_two_vms);

/// A lost device's job half gone: the rig, the memory, what it left.
fn lost_device() -> Result<(Rig, Mem, Held), String> {
    let cfg = JobCfg { timeouts_us: Some([50_000, HOUR_US, HOUR_US]), ladder_from: Rung::None, ..JobCfg::default() };
    let r = rig(cfg)?;
    let m = Mem::new(&r.gpu)?;
    let cx = Cx::new(&r.gpu);
    let e = stuck(&r, &m, &cx, RingType::Gfx, 0x80)?;
    kassert_end(&r.gpu, &e, RingType::Gfx)?;
    let held = r.gpu.stop_jobs().ok_or("no job half")?;
    kassert!(held.len() == 1, "held {}", held.len());
    Ok((r, m, held))
}

/// Held jobs go only after a stop that proved the engines halted. A failed stop, or one that left an engine on, keeps them
/// for the boot, and a later stop that finds no chain vouches for nothing.
pub(crate) fn held_after_stop() -> ktest::TestResult {
    let (r, m, held) = lost_device()?;
    let g = r.gpu.clone();
    let h = HeldJobs::new();
    kassert!(h.hold(Some(held)) == 1 && h.after_stop(Ok(false)) == Err(1) && h.after_stop(Err(())) == Err(1), "a stop that did not pass let go");
    drop(m);
    kassert!(g.vms_alive() == 1, "{} VMs alive", g.vms_alive());
    kassert!(h.after_stop(Ok(true)) == Ok(1) && g.vms_alive() == 0 && h.count() == 0, "a stop that passed: {} VMs alive", g.vms_alive());
    r.finish()?;

    let (r, m, held) = lost_device()?;
    let g = r.gpu.clone();
    let h = HeldJobs::new();
    h.hold(Some(held));
    h.engines_left_running();
    kassert!(h.after_stop(Ok(true)) == Err(1), "an engine left on, and the jobs went");
    // GX-9a: a full reset that passed proves what a stop could not: the flag goes, and what it kept.
    drop(m);
    kassert!(g.vms_alive() == 1, "{} VMs alive", g.vms_alive());
    kassert!(h.engines_proven_halted() == 1 && g.vms_alive() == 0 && h.count() == 0, "{} VMs alive", g.vms_alive());
    kassert!(h.after_stop(Ok(true)) == Ok(0), "the flag stayed");
    drop(h);
    r.finish()?;

    let (r, m, held) = lost_device()?;
    let h = HeldJobs::new();
    h.hold(Some(held));
    h.stop_found_no_chain();
    kassert!(h.after_stop(Ok(true)) == Err(1), "a stop with no chain let go");
    drop(h);
    drop(m);
    r.finish()?;

    // With nothing held, finding no chain marks nothing.
    let h = HeldJobs::new();
    h.stop_found_no_chain();
    kassert!(h.after_stop(Ok(true)) == Ok(0), "an idle stop");
    Ok(())
}
ktest::ktest!("gx.hw.job_held_after_stop", Safe, held_after_stop);

/// A long queue behind one dependency: its entity waits in order, another entity passes it, nothing stays queued.
pub(crate) fn queue_order() -> ktest::TestResult {
    let r = rig(JobCfg::default())?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let cx = Cx::new(g);
    let ty = RingType::Sdma;
    let c = fence::context_alloc(1);
    let outside = Fence::new(c, 1, "test", "outside");
    let waits = cx.entity(ty);
    let mut last = None;
    for k in 0..100u32 {
        let (j, e) = job(waits.clone(), vec![m.ib(&write_ib(ty, DATA, k + 1), 0)]);
        let deps = if k == 0 { vec![outside.clone()] } else { Vec::new() };
        g.submit(sub(g, &m, vec![j], deps, None)).map_err(|x| format!("submit {k}: errno {x}"))?;
        last = Some(e);
    }
    let last = last.ok_or("no job")?;
    let (j, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, DATA + 4, 7), 0)]);
    g.submit(sub(g, &m, vec![j], Vec::new(), None)).map_err(|x| format!("submit: errno {x}"))?;
    g.kick();
    kassert_end(g, &e, ty)?;
    kassert!(status(&e) == 1 && m.gtt_u32(4) == 7, "the other entity did not pass: {}", status(&e));
    kassert!(m.gtt_u32(0) == 0 && !fence::is_signaled(&last.scheduled), "the entity did not wait: {}", m.gtt_u32(0));
    outside.signal();
    g.kick();
    kassert_end(g, &last, ty)?;
    kassert!(status(&last) == 1 && m.gtt_u32(0) == 100, "reads {}", m.gtt_u32(0));
    let jobs = g.jobs().ok_or("no jobs")?;
    let text = jobs.describe().join("\n");
    kassert!(text.contains(" 0 queued"), "{text}");
    drop(jobs);
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.job_queue_order", Safe, queue_order);
