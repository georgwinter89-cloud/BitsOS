//! GX-9a: GR-3's ladder on the fake chain, a rung at a time: the job's waves killed, the queue reset through MES, GC's re-init,
//! mode 2 behind its key, and the device lost when none of it works. Fake engines only: Safe and rerunnable.

use crate::gpucore::mesfake::MesHostile;
use crate::hw::frame::RingType;
use crate::hw::jobs::{JobCfg, Jobs, Rung};
use crate::hw::live::HaltProof;
use crate::ipmod::mes11::f as mes_f;
use crate::ipmod::gmc11::Hub;
use crate::ktests_hw::Rig;
use crate::ktests_hw_jobs::{Cx, DATA, Ends, Mem, give_way, job, kassert_end, rig, stuck_ib, sub, write_ib};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::errno;
use amdgpu_fmt::recover::{MODE2_UNMEASURED, REINIT_LOOP};
use amdgpu_fmt::told::{ContextState, Counters, query2};
use core::sync::atomic::Ordering;
use drm::fence;
use drm::gpu::GpuDevice;
use ktest::kassert;
use linux::ab::errno_more::ECANCELED;
use linux::errno::{ENODEV, ETIME};

const HOUR_US: u64 = 3_600_000_000;
/// How long a test's hang takes to be called one.
const QUICK_US: u64 = 30_000;
/// Where the stuck IB waits, in the GTT object.
const WAITS_AT: u64 = 0x80;

fn cfg_from(from: Rung) -> JobCfg {
    JobCfg { timeouts_us: Some([QUICK_US; 3]), ladder_from: from, ..JobCfg::default() }
}

fn status(e: &Ends) -> i32 {
    fence::status(&e.finished)
}

fn jobs(r: &Rig) -> Result<Arc<Jobs>, String> {
    Ok(r.gpu.jobs().ok_or("no jobs")?)
}

fn rec(j: &Jobs, r: Rung) -> u64 {
    j.shared.recovered[r as usize].load(Ordering::Acquire)
}

fn stuck_words(ty: RingType, va: u64) -> Vec<u32> {
    match ty {
        RingType::Sdma => {
            use amdgpu_fmt::sdma::{PollRegMem, poll_regmem};
            let p = PollRegMem { hdp_flush: false, mem: true, function: 3, addr_lo: va as u32, addr_hi: (va >> 32) as u32, reference: 0x1234_5678, mask: u32::MAX, interval: 10, retry: 0xfff };
            let mut v = Vec::new();
            let _ = poll_regmem(&mut v, &p);
            v.resize(v.len().next_multiple_of(8), 0);
            v
        }
        _ => stuck_ib(va),
    }
}

/// A job that never ends, on its ring.
fn hang(r: &Rig, m: &Mem, cx: &Cx, ty: RingType) -> Result<Ends, String> {
    let g = &r.gpu;
    let (j, e) = job(cx.entity(ty), vec![m.ib(&stuck_words(ty, DATA + WAITS_AT), 0)]);
    g.submit(sub(g, m, vec![j], Vec::new(), None)).map_err(|x| format!("{}: submit: errno {x}", ty.name()))?;
    g.kick();
    Ok(e)
}

/// A job that runs to its end, on each ring: the device took the recovery. Nothing else may be on the rings.
fn all_rings_run(r: &Rig, m: &Mem, cx: &Cx, at: u64) -> ktest::TestResult {
    let g = &r.gpu;
    for (k, ty) in RingType::ALL.into_iter().enumerate() {
        let off = at + 4 * k as u64;
        let v = 0x5a00 + k as u32;
        let (j, e) = job(cx.entity(ty), vec![m.ib(&write_ib(ty, DATA + off, v), 0)]);
        g.submit(sub(g, m, vec![j], Vec::new(), None)).map_err(|x| format!("{}: errno {x}", ty.name()))?;
        g.kick();
        kassert_end(g, &e, ty)?;
        kassert!(status(&e) == 1 && m.gtt_u32(off) == v, "{}: status {}, reads {:#x}", ty.name(), status(&e), m.gtt_u32(off));
    }
    Ok(())
}

/// What QUERY_STATE2 answers for this context, by `told`'s rule: the same as `drm::ctx` (the check list reads that one).
fn told_flags(r: &Rig, cx: &Cx, generation: u32) -> u64 {
    let g = &r.gpu;
    let now = Counters { reset: g.reset_counter(), vram_lost: g.vram_lost_counter(), in_reset: g.in_reset() };
    now.query2(&ContextState { reset_counter: cx.snaps.reset_counter.load(Ordering::Acquire), generation, guilty: cx.is_guilty() })
}

/// Until the job half has done what a fate's waiter does not wait for.
fn settle(r: &Rig, j: &Jobs, rung: Rung) -> ktest::TestResult {
    settle_at(r, j, rung, 1)
}

/// The same for the `n`th time this rung ended a recovery.
fn settle_at(r: &Rig, j: &Jobs, rung: Rung, n: u64) -> ktest::TestResult {
    for _ in 0..4000 {
        if rec(j, rung) >= n {
            return Ok(());
        }
        give_way(&r.gpu, 5_000_000);
    }
    Err(format!("the ladder did not end at {} {n} times: {:?}", rung.name(), j.describe()))
}

/// The job's waves held it: the kill frees it. It ends ENODATA, nobody is guilty, what stood behind it runs.
pub(crate) fn soft() -> ktest::TestResult {
    let r = rig(JobCfg { timeouts_us: Some([QUICK_US, HOUR_US, HOUR_US]), ..JobCfg::default() })?;
    let g = &r.gpu;
    let (m, cx, other) = (Mem::new(g)?, Cx::new(g), Cx::new(g));
    let j = jobs(&r)?;
    // The VMID comes with the job: a kill for any other must free nothing, and the first one on this device gets the first.
    let gtt = m.gtt_handle();
    let vmid = crate::hw::vm::KERNEL_VMIDS.start as u8;
    r.fake(|f| f.sq_release = Some((vmid, Arc::new(move || {
        gtt.write(WAITS_AT, &0x1234_5678u32.to_le_bytes());
    }))));
    let bad = hang(&r, &m, &cx, RingType::Gfx)?;
    let (b, behind) = job(other.entity(RingType::Gfx), vec![m.ib(&write_ib(RingType::Gfx, DATA, 7), 0)]);
    g.submit(sub(g, &m, vec![b], Vec::new(), None)).map_err(|x| format!("errno {x}"))?;
    g.kick();
    kassert_end(g, &bad, RingType::Gfx)?;
    kassert_end(g, &behind, RingType::Gfx)?;
    kassert!(status(&bad) == -errno::ENODATA, "the killed job: {}, kills {:?}, data {:#x}", status(&bad), r.fake(|f| f.mes.sq_cmds.len()), m.gtt_u32(WAITS_AT));
    kassert!(status(&behind) == 1 && m.gtt_u32(0) == 7, "behind it: {}", status(&behind));
    kassert!(!cx.is_guilty() && !other.is_guilty(), "guilt after a kill");
    settle(&r, &j, Rung::Soft)?;
    kassert!(g.reset_counter() == 1, "Linux counts a kill: {}", g.reset_counter());
    let kills = r.fake(|f| f.mes.sq_cmds.clone());
    kassert!(kills.first() == Some(&mes_f::sq_kill(vmid)), "{kills:x?}");
    kassert!(!j.shared.lost.load(Ordering::Acquire), "{:?}", j.describe());
    all_rings_run(&r, &m, &other, 0x10)?;
    r.fake(|f| f.sq_release = None);
    drop(j);
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.ladder_soft", Safe, soft);

/// A kill finds nothing to free, then the queue rung, for each ring. The hung job ETIME, its context guilty, the one behind
/// it ECANCELED and the device's counter moved for it; the other rings never noticed, the ring runs on, the picture stays.
fn queue_on(ty: RingType) -> ktest::TestResult {
    let mut t = [HOUR_US; 3];
    t[ty.index()] = QUICK_US;
    // The job parked on another ring ends at the stop, after `close_us`.
    let r = rig(JobCfg { timeouts_us: Some(t), close_us: 50_000, ..JobCfg::default() })?;
    let g = &r.gpu;
    let (m, cx, other, far) = (Mem::new(g)?, Cx::new(g), Cx::new(g), Cx::new(g));
    let j = jobs(&r)?;
    let elsewhere = RingType::ALL.into_iter().find(|&x| x != ty).ok_or("one ring")?;
    let far_mem = Mem::new(g)?;
    let (x, parked) = job(far.entity(elsewhere), vec![far_mem.ib(&stuck_words(elsewhere, DATA + WAITS_AT), 0)]);
    g.submit(sub(g, &far_mem, vec![x], Vec::new(), None)).map_err(|e| format!("errno {e}"))?;
    let bad = hang(&r, &m, &cx, ty)?;
    let (b, behind) = job(other.entity(ty), vec![m.ib(&write_ib(ty, DATA, 7), 0)]);
    g.submit(sub(g, &m, vec![b], Vec::new(), None)).map_err(|x| format!("errno {x}"))?;
    g.kick();
    kassert_end(g, &bad, ty)?;
    kassert_end(g, &behind, ty)?;
    kassert!(status(&bad) == -(ETIME as i32) && status(&behind) == -(ECANCELED as i32), "fates {} {}", status(&bad), status(&behind));
    kassert!(cx.is_guilty() && !other.is_guilty() && !far.is_guilty(), "guilt");
    settle(&r, &j, Rung::Queue)?;
    kassert!(g.reset_counter() >= 1, "the counter did not move");
    kassert!(told_flags(&r, &cx, 0) & (query2::RESET | query2::GUILTY) == query2::RESET | query2::GUILTY, "told: {:#x}", told_flags(&r, &cx, 0));
    kassert!(!fence::is_signaled(&parked.finished), "another ring's job noticed: {}", fence::status(&parked.finished));
    kassert!(!j.shared.lost.load(Ordering::Acquire) && rec(&j, Rung::Gc) == 0, "{:?}", j.describe());
    // The ring runs on: a job of the context that lost one, in the same VM.
    let (b, again) = job(other.entity(ty), vec![m.ib(&write_ib(ty, DATA + 4, 9), 0)]);
    g.submit(sub(g, &m, vec![b], Vec::new(), None)).map_err(|x| format!("errno {x}"))?;
    g.kick();
    kassert_end(g, &again, ty)?;
    kassert!(status(&again) == 1 && m.gtt_u32(4) == 9, "the ring does not run on: {}", status(&again));
    let text = j.describe().join("\n");
    kassert!(text.contains("the last hang") && text.contains("1 by a queue reset") && text.contains("display writes while the backend ran: 0"), "{text}");
    // The job we parked ends at the stop.
    drop(j);
    let held = g.stop_jobs().ok_or("no job half")?;
    drop(held);
    kassert!(r.fake(|f| f.mes.unknown_removes) == 0, "a REMOVE_QUEUE for a queue MES does not hold");
    drop((m, far_mem));
    r.finish()
}

pub(crate) fn queue_gfx() -> ktest::TestResult {
    queue_on(RingType::Gfx)
}
pub(crate) fn queue_compute() -> ktest::TestResult {
    queue_on(RingType::Compute)
}
pub(crate) fn queue_sdma() -> ktest::TestResult {
    queue_on(RingType::Sdma)
}
ktest::ktest!("gx.hw.ladder_queue_gfx", Safe, queue_gfx);
ktest::ktest!("gx.hw.ladder_queue_compute", Safe, queue_compute);
ktest::ktest!("gx.hw.ladder_queue_sdma", Safe, queue_sdma);

/// REMOVE_QUEUE answers and leaves ring 0's HQD active: the reset dequeue through CP_GFX_HQD_*, as 7.1. One that stays active
/// has no firmware owning it: no second REMOVE_QUEUE at the stop, and the ladder has nothing left.
pub(crate) fn gfx_hqd() -> ktest::TestResult {
    let r = rig(JobCfg { timeouts_us: Some([QUICK_US, HOUR_US, HOUR_US]), ..JobCfg::default() })?;
    r.fake(|f| f.mes.hostile = MesHostile::UnmapLeavesActive);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::Queue)?;
    kassert!(r.fake(|f| f.mes.resets.len()) == 1, "resets {}", r.fake(|f| f.mes.resets.len()));
    r.fake(|f| f.mes.hostile = MesHostile::None);
    all_rings_run(&r, &m, &Cx::new(g), 0x10)?;
    drop(j);
    drop(m);
    r.finish_jobs()?;

    let r = rig(JobCfg { timeouts_us: Some([QUICK_US, HOUR_US, HOUR_US]), ..JobCfg::default() })?;
    r.fake(|f| f.mes.hostile = MesHostile::HqdStuck);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(j.shared.lost.load(Ordering::Acquire), "{:?}", j.describe());
    kassert!(r.fake(|f| f.mes.unknown_removes) == 0, "a REMOVE_QUEUE for a queue MES does not hold");
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_gfx_hqd", Safe, gfx_hqd);

/// A queue mapped again that never runs: the ring test says so and the ladder goes on to GC's re-init, which brings it back.
pub(crate) fn queue_dead() -> ktest::TestResult {
    let r = rig(JobCfg { timeouts_us: Some([QUICK_US, HOUR_US, HOUR_US]), ..JobCfg::default() })?;
    r.fake(|f| f.mes.hostile = MesHostile::RemapDead);
    r.reset.knobs.heals.store(true, Ordering::Release);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::Gc)?;
    kassert!(status(&e) == -(ETIME as i32) && rec(&j, Rung::Queue) == 0, "{}: {:?}", status(&e), j.describe());
    all_rings_run(&r, &m, &Cx::new(g), 0x10)?;
    drop(j);
    drop(m);
    r.finish_jobs()
}
ktest::ktest!("gx.hw.ladder_queue_dead", Safe, queue_dead);

/// A REMOVE_QUEUE MES never answers: no queue rung; the stop of the re-init cannot prove the engines halted either, so no
/// re-init: the device is lost, cleanly, and the jobs ended.
pub(crate) fn unmap_wedged() -> ktest::TestResult {
    let r = rig(JobCfg { timeouts_us: Some([QUICK_US, HOUR_US, HOUR_US]), ..JobCfg::default() })?;
    r.fake(|f| f.mes.hostile = MesHostile::RemoveHangs);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(status(&e) == -(ETIME as i32) && j.shared.lost.load(Ordering::Acquire), "{}: {:?}", status(&e), j.describe());
    kassert!(r.reset.knobs.proven.load(Ordering::Acquire) == 0, "the failed reset proved something");
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_unmap_wedged", Safe, unmap_wedged);

/// GC's re-init: every job on every ring ends, the chain comes back from the PSP on all three queues, the VMIDs flush again,
/// what the hung jobs reached goes back, the picture stays.
pub(crate) fn gc() -> ktest::TestResult {
    let r = rig(cfg_from(Rung::Gc))?;
    let g = &r.gpu;
    let (m, cx, other) = (Mem::new(g)?, Cx::new(g), Cx::new(g));
    let j = jobs(&r)?;
    let on_gfx = hang(&r, &m, &cx, RingType::Gfx)?;
    let far_mem = Mem::new(g)?;
    let on_compute = hang(&r, &far_mem, &other, RingType::Compute)?;
    let flushes = |r: &Rig| r.fake(|f| f.hubs.requests.iter().filter(|(h, e, _)| *h == Hub::Gfx && *e == RingType::Gfx.q().engine()).count());
    kassert_end(g, &on_gfx, RingType::Gfx)?;
    kassert_end(g, &on_compute, RingType::Compute)?;
    kassert!(status(&on_gfx) == -(ETIME as i32) || status(&on_compute) == -(ETIME as i32), "no job ended ETIME: {} {}", status(&on_gfx), status(&on_compute));
    settle(&r, &j, Rung::Gc)?;
    let k = &r.reset.knobs;
    kassert!(k.reinits.load(Ordering::Acquire) == 1 && k.proven.load(Ordering::Acquire) == 1, "re-inits {}, proven {}", k.reinits.load(Ordering::Acquire), k.proven.load(Ordering::Acquire));
    kassert!(!j.shared.lost.load(Ordering::Acquire) && !g.in_reset(), "{:?}", j.describe());
    kassert!(g.reset_counter() >= 1, "the counter did not move");
    let before = flushes(&r);
    let fresh = Cx::new(g);
    all_rings_run(&r, &m, &fresh, 0x10)?;
    kassert!(flushes(&r) > before, "no flush after the reset: the VMID kept its old claim");
    kassert!(r.fake(|f| f.gc.resets) == 0, "a re-init is no mode 2");
    drop(j);
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.is_empty(), "the reset proved the engines halted, and {} jobs stay held", held.len());
    kassert!(g.vms_alive() == 2, "{} VMs", g.vms_alive());
    drop((m, far_mem));
    kassert!(g.vms_alive() == 0, "{} VMs alive", g.vms_alive());
    r.finish()
}
ktest::ktest!("gx.hw.ladder_gc", Safe, gc);

/// The limit of a window: a program restarting into the same hang must not keep GC in a loop: three, then the device is lost.
pub(crate) fn gc_limit() -> ktest::TestResult {
    let r = rig(cfg_from(Rung::Gc))?;
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let j = jobs(&r)?;
    for n in 1..=3u64 {
        let cx = Cx::new(g);
        let e = hang(&r, &m, &cx, RingType::Gfx)?;
        kassert_end(g, &e, RingType::Gfx)?;
        kassert!(status(&e) == -(ETIME as i32), "{n}: {}", status(&e));
        settle_at(&r, &j, Rung::Gc, n)?;
    }
    let cx = Cx::new(g);
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(j.shared.lost.load(Ordering::Acquire) && r.reset.knobs.reinits.load(Ordering::Acquire) == 3, "{:?}", j.describe());
    let text = j.describe().join("\n");
    kassert!(text.contains(REINIT_LOOP), "{text}");
    let (x, y) = job(cx.entity(RingType::Sdma), vec![m.ib(&write_ib(RingType::Sdma, DATA, 1), 0)]);
    kassert!(g.submit(sub(g, &m, vec![x], Vec::new(), None)) == Err(ENODEV) && status(&y) == -(ENODEV as i32), "a lost device took a job");
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_gc_limit", Safe, gc_limit);

/// Mode 2 closed, the default: nothing is sent, a hang past the re-init ends the device, and the tool says why.
pub(crate) fn mode2_closed() -> ktest::TestResult {
    let r = rig(cfg_from(Rung::Mode2))?;
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    let k = &r.reset.knobs;
    kassert!(k.suspends.load(Ordering::Acquire) == 0 && k.mode2s.load(Ordering::Acquire) == 0 && k.reinits.load(Ordering::Acquire) == 0, "a rung ran");
    kassert!(r.fake(|f| f.gc.resets) == 0, "the message went");
    let text = j.describe().join("\n");
    kassert!(text.contains(MODE2_UNMEASURED) && text.contains("LOST"), "{text}");
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_mode2_closed", Safe, mode2_closed);

/// `gx_mode2=on`: the suspend, the one message, the re-init, all rings tested. One SMU message, never mode 1.
pub(crate) fn mode2_open() -> ktest::TestResult {
    let r = rig(JobCfg { mode2: Some("a test"), ..cfg_from(Rung::Mode2) })?;
    r.fake(|f| f.smu.mode2_answers = true);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::Mode2)?;
    kassert!(r.fake(|f| f.gc.resets) == 1, "resets {}", r.fake(|f| f.gc.resets));
    let k = &r.reset.knobs;
    kassert!(k.suspends.load(Ordering::Acquire) == 1 && k.mode2s.load(Ordering::Acquire) == 1 && k.reinits.load(Ordering::Acquire) == 1, "{:?}", j.describe());
    kassert!(!j.shared.lost.load(Ordering::Acquire), "{:?}", j.describe());
    all_rings_run(&r, &m, &Cx::new(g), 0x10)?;
    drop(j);
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.is_empty(), "{} held", held.len());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_mode2_open", Safe, mode2_open);

/// A PMFW that does not answer the message, as 13.0.4 on 28.10.: the rung fails, the device is lost, the jobs ended.
pub(crate) fn mode2_no_answer() -> ktest::TestResult {
    let r = rig(JobCfg { mode2: Some("a test"), ..cfg_from(Rung::Mode2) })?;
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(j.shared.lost.load(Ordering::Acquire) && status(&e) == -(ETIME as i32), "{:?}", j.describe());
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_mode2_no_answer", Safe, mode2_no_answer);

/// `gx_mode2=on`: the re-init's stop could not halt an engine and its chain is gone: the suspend that finds no chain must
/// not take that for a proof, and the one SMU message is never sent.
pub(crate) fn mode2_unproven() -> ktest::TestResult {
    let r = rig(JobCfg { mode2: Some("a test"), ..cfg_from(Rung::Gc) })?;
    r.fake(|f| f.smu.mode2_answers = true);
    let k = &r.reset.knobs;
    k.halt_fails.store(true, Ordering::Release);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(k.reinits.load(Ordering::Acquire) == 1 && k.suspends.load(Ordering::Acquire) == 1, "{:?}", j.describe());
    kassert!(k.mode2s.load(Ordering::Acquire) == 0 && r.fake(|f| f.gc.resets) == 0, "mode 2 on an engine not halted: {:?}", j.describe());
    kassert!(j.shared.lost.load(Ordering::Acquire) && k.proven.load(Ordering::Acquire) == 0 && status(&e) == -(ETIME as i32), "{:?}", j.describe());
    kassert!(Arc::strong_count(&m.vm) > 1, "the hung job's VM went");
    drop(j);
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.len() == 1, "held {}", held.len());
    drop(held);
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_mode2_unproven", Safe, mode2_unproven);

/// What a stop with no chain may say: a failed stop stays failed until a chain with both engines halts them.
pub(crate) fn halt_proof() -> ktest::TestResult {
    let p = HaltProof::new();
    kassert!(p.proven(), "nothing failed yet");
    p.stop_ran(true, false);
    kassert!(p.proven(), "an early stage's good stop");
    p.stop_ran(false, true);
    kassert!(!p.proven(), "a failed stop");
    p.stop_ran(true, false);
    kassert!(!p.proven(), "a chain that held neither engine speaks for nothing");
    p.stop_ran(true, true);
    kassert!(p.proven(), "both engines halted");
    p.halt_failed();
    kassert!(!p.proven(), "a run's own halt failed");
    Ok(())
}
ktest::ktest!("gx.hw.ladder_halt_proof", Safe, halt_proof);

/// A re-init that fails: the device is lost for this boot, the jobs ended, what they reach stays: nothing proved it halted.
pub(crate) fn gc_fails() -> ktest::TestResult {
    let r = rig(cfg_from(Rung::Gc))?;
    r.reset.knobs.reinit_fails.store(1, Ordering::Release);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(j.shared.lost.load(Ordering::Acquire) && !g.in_reset(), "{:?}", j.describe());
    kassert!(r.reset.knobs.proven.load(Ordering::Acquire) == 0, "a failed reset proved something");
    kassert!(Arc::strong_count(&m.vm) > 1, "the hung job's VM went");
    // What starts now finds no device, as after a boot whose bring-up failed.
    let dev = drm::add_device(g.clone()).map_err(|x| format!("add: errno {x}"))?;
    let opened = drm::node::open_device(dev.clone(), drm::file::MinorKind::Render, false).map(|_| ());
    drm::remove_device(&dev);
    kassert!(opened == Err(ENODEV), "a lost device opened: {opened:?}");
    drop(j);
    let held = g.stop_jobs().ok_or("no job half")?;
    kassert!(held.len() == 1, "held {}", held.len());
    drop(held);
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_gc_fails", Safe, gc_fails);

/// A reset that moved the picture: GX-D9 was broken. This one ends as it went; there is no next one in this boot.
pub(crate) fn picture_moved() -> ktest::TestResult {
    let r = rig(cfg_from(Rung::Gc))?;
    r.reset.knobs.picture_moves.store(true, Ordering::Release);
    let g = &r.gpu;
    let m = Mem::new(g)?;
    let j = jobs(&r)?;
    let cx = Cx::new(g);
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::Gc)?;
    let cx = Cx::new(g);
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(r.reset.knobs.reinits.load(Ordering::Acquire) == 1, "a second re-init after the picture moved");
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_picture_moved", Safe, picture_moved);

/// VRAM that did not hold: the counter tells every context, and the device stays down.
pub(crate) fn vram_lost() -> ktest::TestResult {
    let r = rig(cfg_from(Rung::Gc))?;
    r.reset.knobs.wipes_vram.store(true, Ordering::Release);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(g.vram_lost_counter() == 1, "counter {}", g.vram_lost_counter());
    kassert!(told_flags(&r, &cx, 0) & query2::VRAMLOST != 0, "told: {:#x}", told_flags(&r, &cx, 0));
    kassert!(j.shared.lost.load(Ordering::Acquire), "{:?}", j.describe());
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
ktest::ktest!("gx.hw.ladder_vram_lost", Safe, vram_lost);

/// A re-init that does not answer: the clock's `expire` gives up for it, the jobs end while it still sits there, its late
/// answer means nothing and it takes nothing with it: it stops at its next look, before the stop and the stages, and says so.
/// A thread is a kernel matter: no host form.
#[cfg(not(test))]
fn expires() -> ktest::TestResult {
    let r = rig(JobCfg { reinit_us: Some(10_000), expire_slack_us: Some(10_000), ..cfg_from(Rung::Gc) })?;
    let k = &r.reset.knobs;
    k.reinit_holds.store(true, Ordering::Release);
    k.reinit_fails.store(1, Ordering::Release);
    let g = &r.gpu;
    let (m, cx) = (Mem::new(g)?, Cx::new(g));
    let j = jobs(&r)?;
    let seq = klog::next_seq();
    let e = hang(&r, &m, &cx, RingType::Gfx)?;
    kassert_end(g, &e, RingType::Gfx)?;
    settle(&r, &j, Rung::None)?;
    kassert!(k.reinit_ended.load(Ordering::Acquire) == 0, "the thread ended while the test held it");
    kassert!(j.shared.lost.load(Ordering::Acquire) && status(&e) == -(ETIME as i32), "{:?}", j.describe());
    kassert!(k.abandoned.load(Ordering::Acquire), "the chain was not told");
    k.reinit_holds.store(false, Ordering::Release);
    let said = || klog::read_since(seq, usize::MAX).iter().any(|r| r.name == "hw.reset_late_answer");
    for _ in 0..2000 {
        if said() {
            break;
        }
        sched::sleep_ns(5_000_000);
    }
    kassert!(k.reinit_ended.load(Ordering::Acquire) == 1, "the thread never ended");
    kassert!(said(), "no event for the late answer");
    kassert!(k.reinit_fails.load(Ordering::Acquire) == 1, "the late thread went on to the stages");
    kassert!(k.proven.load(Ordering::Acquire) == 0, "a late answer proved something");
    drop(j);
    drop(g.stop_jobs());
    drop(m);
    r.finish()
}
#[cfg(not(test))]
ktest::ktest!("gx.hw.ladder_expire", Safe, expires);

/// CONFIG `gx_mode2`: on opens it, every other word leaves it closed.
pub(crate) fn config() -> ktest::TestResult {
    use crate::hw::boot::read_mode2;
    let of = |v: Option<&'static str>| read_mode2(&move |k| (k == "gx_mode2").then(|| v.map(String::from)).flatten());
    kassert!(of(Some("on")) && of(Some(" on ")), "on");
    kassert!(!of(None) && !of(Some("")) && !of(Some("off")) && !of(Some("1")) && !of(Some("ON")), "only on opens it");
    Ok(())
}
ktest::ktest!("gx.hw.ladder_config", Safe, config);

/// The tool's word, and what it leaves the chip with: nothing.
pub(crate) fn tool_word() -> ktest::TestResult {
    use crate::psplive::{Action, parse_args};
    for (w, want) in [("soft", Rung::Soft), ("queue", Rung::Queue), ("gc", Rung::Gc), ("mode2", Rung::Mode2), ("none", Rung::None)] {
        let a = mcp::Json::parse(&format!("{{\"until\":\"ladder\",\"from\":\"{w}\"}}")).map_err(|e| format!("{e:?}"))?;
        kassert!(parse_args(&a).map(|x| x.action) == Ok(Action::Ladder(want)), "{w}");
    }
    let a = mcp::Json::parse("{\"until\":\"ladder\"}").map_err(|e| format!("{e:?}"))?;
    kassert!(parse_args(&a).is_err(), "no from");
    Ok(())
}
ktest::ktest!("gx.hw.ladder_tool", Safe, tool_word);
