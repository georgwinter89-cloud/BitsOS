//! GX-8b: what the job thread does each round: poll, end jobs, pick, write frames. `lost` holds the path of a fault or
//! a hang. The chain's lock is taken for each step alone and never across a signal: a fence's callbacks can run anything.

use super::chain::Chain;
use super::frame::{FIXED_DW, Frame, HEAD_DW, RingType};
use super::jobs::{CANCELED, JobCfg, Pend, Round, Shared};
use super::rings::{Hold, HwRing, OnRing};
use super::heap::Heaps;
use super::vm::{HwVm, VmIds, VmJob};
use crate::gpucore::vmring::{Queue, VmError, VmInputs};
use crate::ipmod::gmc11::CONTEXTS;
use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use amdgpu_fmt::recover::ResetDomain;
use amdgpu_fmt::ring::RingError;
use amdgpu_fmt::watch::{JobMeta, Owner};
use core::sync::atomic::Ordering;
use drm::fence;
use linux::errno::{EIO, ENODEV};
use vfs::Errno;

/// A frame's end: what to tell once the chain's lock is gone.
pub(super) struct Ended {
    pub on: OnRing,
    pub errno: Option<i32>,
}

pub(crate) struct Inner {
    /// GX-7n: the fences raise interrupts.
    int: bool,
    pub(super) chain: Arc<dyn Chain>,
    pub(super) vmi: VmInputs,
    pub(super) domain: ResetDomain,
    pub(super) rings: [HwRing; 3],
    /// Per VMID: the flush last put on a ring (ring index, its job's number). `id->last_flush` (amdgpu_ids.c).
    last_flush: [Option<(usize, u64)>; CONTEXTS as usize],
    pub(super) cfg: JobCfg,
    pub(super) shared: Arc<Shared>,
    /// Queued submissions in order, moved out of `Shared::pending`: this thread's alone, so scanning holds no lock.
    pub(super) queue: Vec<Pend>,
    max_wave_id: u32,
    /// Jobs told to have ended that the hardware may still run: what they reach stays (see `lost`).
    pub(super) grave: Vec<OnRing>,
    close_at: Option<u64>,
    pub(super) last_hang: Option<String>,
    pub(super) last_fault: Option<(u64, u32)>,
    /// A queue the ladder rebuilt, per ring, until its ring test.
    pub(super) fresh: [Option<Queue>; 3],
    /// GX-9a: a full reset's step on its thread, one the clock gave up on, the page that proves VRAM, whose rung runs, what
    /// the reset's steps said. See `rungs`.
    pub(super) long: Option<super::rungs::Long>,
    pub(super) stale_long: Option<super::rungs::Slot>,
    pub(super) magic: Option<super::rungs::Magic>,
    pub(super) mode2_rung: bool,
    pub(super) reset_lines: Vec<String>,
    pub(super) last_reset: Option<String>,
    pub(super) heaps: Arc<Heaps>,
    pub(super) ids: Arc<VmIds>,
}

/// What became of a submission's try at the rings.
enum Tried {
    Done,
    /// Not now (a ring is full, no VMID): it stays queued.
    Later(Pend),
}

pub(super) fn now_us() -> u64 {
    super::clock_ns() / 1000
}

/// The VM behind a submission, or why not.
fn hw_vm(s: &drm::job::Submission) -> Option<Arc<HwVm>> {
    s.vm.as_any()?.downcast_ref::<HwVm>()?.arc()
}

/// `amdgpu_job`'s end: the user fence before the finished fence. The CPU writes it: Linux's GPU write needs the object in
/// GART (`amdgpu_ttm_alloc_gart`), a slot and a VMID 0 flush per page, for a value only the CPU reads; the order toward
/// `finished` is the same. Only a leader that ran gets here without an errno. What comes back is `let_go` once every job
/// of the batch has signaled: that takes a VM's lock, and a batch's later fences must not wait for it.
pub(super) fn finish(e: Ended, shared: &Shared) -> OnRing {
    let Ended { on, errno } = e;
    let sub = &on.hold.sub;
    let j = &sub.jobs[on.member];
    match errno {
        None => {
            if on.member + 1 == sub.jobs.len()
                && let Some(uf) = &sub.user_fence
                && !uf.mem.write(uf.offset, &uf.value.to_le_bytes())
            {
                klog::event!(Warn, "gx", "hw.user_fence_failed", offset = uf.offset);
            }
            j.finished.signal();
        }
        Some(x) => {
            j.finished.signal_error(x);
        }
    }
    shared.jobs_done.fetch_add(1, Ordering::Relaxed);
    on
}

/// The VM's hold first, then the objects it reaches.
pub(super) fn let_go(gone: Vec<OnRing>) {
    for mut on in gone {
        drop(on.vm.take());
    }
}

impl Inner {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(chain: Arc<dyn Chain>, vmi: VmInputs, domain: ResetDomain, rings: [HwRing; 3], max_wave_id: u32, cfg: JobCfg, shared: Arc<Shared>, int: bool, heaps: Arc<Heaps>, ids: Arc<VmIds>) -> Inner {
        Inner {
            int,
            chain,
            vmi,
            domain,
            rings,
            last_flush: [None; CONTEXTS as usize],
            cfg,
            shared,
            queue: Vec::new(),
            max_wave_id,
            grave: Vec::new(),
            close_at: None,
            last_hang: None,
            last_fault: None,
            fresh: Default::default(),
            long: None,
            stale_long: None,
            magic: None,
            mode2_rung: false,
            reset_lines: Vec::new(),
            last_reset: None,
            heaps,
            ids,
        }
    }

    /// A full reset's step runs.
    pub(super) fn long_running(&self) -> bool {
        self.long.is_some()
    }

    /// After a full reset no ring has flushed anything.
    pub(super) fn forget_flushes(&mut self) {
        self.last_flush = [None; CONTEXTS as usize];
    }

    pub(super) fn take_grave(&mut self) -> Vec<OnRing> {
        core::mem::take(&mut self.grave)
    }

    pub(super) fn round(&mut self) -> Round {
        let now = now_us();
        if self.long.is_some() {
            let moved = self.long_round(now);
            // The chain is the reset's until its step answers, and a stop waits for that too: `expire` bounds it.
            if self.long.is_some() {
                return Round::Busy { moved, stalling: true };
            }
        }
        let mut moved = self.poll(now);
        let closing = self.shared.closing.load(Ordering::Acquire);
        if closing || self.shared.lost.load(Ordering::Acquire) {
            self.cancel_queued();
        } else if self.pump(now) {
            moved = true;
        }
        let busy = self.rings.iter().any(|r| !r.jobs.is_empty());
        if closing {
            if !busy {
                return Round::Exit;
            }
            let at = *self.close_at.get_or_insert(now + self.cfg.close_us);
            if now >= at {
                self.abandon();
                return Round::Exit;
            }
        }
        // Ready but not placed (a VMID, a full ring): only a job's end can change that, and those end on a ring.
        // A stopped compute queue has no interrupt to come: its grace is the poll's business.
        let stalling = self.rings.iter().any(|r| r.stall.is_some());
        if busy { Round::Busy { moved, stalling } } else { Round::Idle }
    }

    /// What `submit` added since the last look; the lock only for the swap.
    fn take_new(&mut self) {
        let new = core::mem::take(&mut *self.shared.pending.lock());
        self.queue.extend(new);
    }

    /// The queue ends unrun.
    fn cancel_queued(&mut self) {
        self.take_new();
        let q = core::mem::take(&mut self.queue);
        if !q.is_empty() {
            klog::event!(Info, "gx", "hw.jobs_canceled", submissions = q.len(); "the device is lost or going: queued jobs end unrun");
        }
        for p in q {
            p.sub.cancel(CANCELED);
        }
    }

    /// `stop`'s time is up: what is still on a ring ends ECANCELED and stays held until the engines are halted.
    fn abandon(&mut self) {
        for r in &mut self.rings {
            while let Some(on) = r.jobs.pop_front() {
                klog::event!(Warn, "gx", "hw.job_abandoned", ring = r.ty.name(), seq = on.seq);
                on.hold.sub.jobs[on.member].finished.signal_error(CANCELED as i32);
                self.grave.push(on);
            }
        }
    }

    /// The next submission to try: ready, not behind an earlier one of its entities, the highest priority, the oldest
    /// among equals. A dependency on a job already on the only ring of a one-job submission counts as met, with a
    /// pipeline sync in the frame: `drm_sched_entity_add_dependency_cb` waits only for "scheduled" there, and
    /// `amdgpu_cs_sync_rings` makes it the job's explicit sync.
    fn pick(&self) -> Option<usize> {
        let mut seen: BTreeSet<u64> = BTreeSet::new();
        let mut best: Option<usize> = None;
        for (i, p) in self.queue.iter().enumerate() {
            let blocked = p.sub.jobs.iter().any(|j| seen.contains(&j.entity.id()));
            seen.extend(p.sub.jobs.iter().map(|j| j.entity.id()));
            if blocked || p.later || !self.deps_met(&p.sub) {
                continue;
            }
            if best.is_none_or(|b| self.queue[b].prio < p.prio) {
                best = Some(i);
            }
        }
        best
    }

    fn deps_met(&self, s: &drm::job::Submission) -> bool {
        s.deps.iter().all(|d| fence::is_signaled(d) || self.on_own_ring(s, d))
    }

    fn on_own_ring(&self, s: &drm::job::Submission, d: &Arc<fence::Fence>) -> bool {
        let [only] = &s.jobs[..] else { return false };
        RingType::of_ip(only.entity.ip_type).is_some_and(|t| self.rings[t.index()].jobs.iter().any(|on| Arc::ptr_eq(&on.hold.sub.jobs[on.member].finished, d)))
    }

    /// Everything ready goes on the rings; true when something did.
    fn pump(&mut self, now: u64) -> bool {
        self.take_new();
        let mut any = false;
        for p in &mut self.queue {
            p.later = false;
        }
        while let Some(i) = self.pick() {
            let p = self.queue.remove(i);
            match self.place(p, now) {
                Tried::Done => any = true,
                Tried::Later(mut p) => {
                    p.later = true;
                    self.queue.insert(i, p);
                }
            }
        }
        any
    }

    /// The gang's members go on their rings together or, when one has no room or no VMID, not at all.
    fn place(&mut self, p: Pend, now: u64) -> Tried {
        let lost_generation = self.shared.vram_lost.load(Ordering::Acquire);
        let refused: Vec<Option<Errno>> = p.sub.jobs.iter().map(|j| j.entity.refused().or((p.sub.generation != lost_generation).then_some(CANCELED))).collect();
        let running: Vec<(usize, RingType)> = p.sub.jobs.iter().enumerate().filter(|(i, _)| refused[*i].is_none()).filter_map(|(i, j)| RingType::of_ip(j.entity.ip_type).map(|t| (i, t))).collect();
        if !self.room_for(&p.sub, &running) {
            return Tried::Later(p);
        }
        let mut vms: Vec<Result<VmJob, Errno>> = Vec::new();
        if !running.is_empty() {
            let Some(vm) = hw_vm(&p.sub) else {
                // A configuration mismatch, not a fault of the program.
                klog::event!(Error, "gx", "hw.job_no_vm", jobs = p.sub.jobs.len(); "the submission's VM is not this backend's");
                p.sub.cancel(ENODEV);
                return Tried::Done;
            };
            for _ in &running {
                match vm.begin_job() {
                    Ok(Some(j)) => vms.push(Ok(j)),
                    Ok(None) => {
                        for j in vms.into_iter().flatten() {
                            j.cancel();
                        }
                        return Tried::Later(p);
                    }
                    Err(e) => vms.push(Err(e)),
                }
            }
        }
        let same_ring = p.sub.deps.iter().any(|d| !fence::is_signaled(d));
        let Pend { sub, regs, .. } = p;
        drop(regs);
        let hold = Arc::new(Hold { sub });
        let sub = &hold.sub;
        for (i, r) in refused.iter().enumerate() {
            if let Some(e) = r {
                klog::event!(Info, "gx", "hw.job_unrun", ip = sub.jobs[i].entity.ip_type, errno = *e; "a job of a guilty, ended or VRAM-lost context ends unrun");
                sub.jobs[i].scheduled.signal();
                sub.jobs[i].finished.signal_error(*e as i32);
            }
        }
        // `grab` wrote a flush down as done for the member that got it, so a later member sharing the VMID was told none.
        // A member that fails took that flush with it: the rest of the gang must do it, `forget` only reaches later grabs.
        let mut flush_lost = false;
        for ((member, ty), vm) in running.into_iter().zip(vms) {
            let j = &sub.jobs[member];
            let mut vm = match vm {
                Ok(v) => v,
                Err(e) => {
                    klog::event!(Warn, "gx", "hw.job_no_vmid", errno = e);
                    j.scheduled.signal();
                    j.finished.signal_error(e as i32);
                    continue;
                }
            };
            j.scheduled.signal();
            if self.shared.lost.load(Ordering::Acquire) {
                // A member's frame just lost the device: no more words on these queues.
                j.finished.signal_error(CANCELED as i32);
                vm.cancel();
                continue;
            }
            vm.needs_flush |= flush_lost;
            if let Err((e, unused)) = self.put_on_ring(&hold, member, ty, vm, same_ring, now) {
                flush_lost = true;
                j.finished.signal_error(e as i32);
                if let Some(vm) = unused {
                    vm.cancel();
                }
            }
        }
        Tried::Done
    }

    /// The read pointer room is judged by. With no job on the ring it has read everything, whatever its last write-back
    /// said (nothing polls an idle ring, and a job must not wait for a poll that will not come).
    fn rptr_of(&self, r: &HwRing) -> u64 {
        match self.domain.ring(r.id) {
            Some(w) if r.jobs.is_empty() => w.wptr(),
            Some(w) => w.rptr(),
            None => r.queue.wptr(),
        }
    }

    /// Room for each member, its head's largest size counted: a flush and a sync are always possible. The largest frame
    /// CS lets through fits an empty ring (cs.rs `max_ibs`: 2048 dwords, at most 875 of them).
    fn room_for(&self, sub: &drm::job::Submission, running: &[(usize, RingType)]) -> bool {
        running.iter().all(|&(i, t)| {
            let r = &self.rings[t.index()];
            let Some(w) = self.domain.ring(r.id) else { return false };
            w.timeline().in_flight() < w.cfg().max_in_flight && r.queue.fits(HEAD_DW + FIXED_DW + t.ib_dw() * sub.jobs[i].ibs.len() as u64, self.rptr_of(r))
        })
    }

    /// One frame: the flush this job's VMID wants, the ring's sync, the IBs, the fence. The watch hears of it after the
    /// doorbell: this thread is the only reader of the ring, so no poll falls between. An `Err` gives the `VmJob` back when
    /// nothing reached the ring: the caller cancels it after the job's end.
    fn put_on_ring(&mut self, hold: &Arc<Hold>, member: usize, ty: RingType, mut vm: VmJob, deps: bool, now: u64) -> Result<(), (Errno, Option<VmJob>)> {
        let ri = ty.index();
        let vmid = vm.vmid as u8;
        // Linux's rule (amdgpu_vmid_grab_used, amdgpu_ids.c): the flush of another ring that has not signaled is no flush
        // for this ring yet; flush here too.
        let other = self.last_flush[vmid as usize & 0xf].is_some_and(|(r, seq)| r != ri && self.domain.ring(self.rings[r].id).is_some_and(|w| !w.timeline().is_signalled(seq)));
        vm.needs_flush |= other;
        let j = &hold.sub.jobs[member];
        let (rid, fence_mc) = (self.rings[ri].id, self.rings[ri].fence);
        let Some(w) = self.domain.ring(rid) else {
            return Err((ENODEV, Some(vm)));
        };
        let (last, seq) = (w.timeline().emitted(), w.timeline().emitted().wrapping_add(1));
        let rptr = self.rptr_of(&self.rings[ri]);
        let fctx = j.entity.fence_context;
        let sync = (vm.needs_flush || deps).then_some((fence_mc, last));
        let frame = Frame {
            ty,
            addrs: &self.vmi.addrs,
            ibs: &j.ibs,
            vmid,
            flush: vm.needs_flush.then_some((vm.pd, vm.pasid)),
            sync,
            ctx_switch: self.rings[ri].current_ctx != Some(fctx),
            fence: (fence_mc, seq),
            max_wave_id: self.max_wave_id,
            int: self.int,
        };
        let head = match frame.head() {
            Ok(h) => h,
            Err(e) => {
                klog::event!(Error, "gx", "hw.frame_failed", why = format!("{e:?}").as_str());
                return Err((EIO, Some(vm)));
            }
        };
        let ndw = frame.bound(&head);
        let start = self.rings[ri].queue.wptr();
        self.rings[ri].queue.set_rptr(rptr);
        let queue = &mut self.rings[ri].queue;
        let mut res: Option<Result<u64, VmError>> = None;
        let mut bells = Vec::new();
        self.chain.with(&mut |o| {
            res = Some(queue.submit(o.io(), ndw, |g| frame.emit(&head, g), &mut bells));
        });
        let end = match res {
            Some(Ok(end)) => end,
            Some(Err(VmError::Ring(RingError::Full { need, free }))) => {
                // `room_for` counted more than any frame takes: a bug, and the job must still end.
                klog::event!(Error, "gx", "hw.ring_full", ring = ty.name(), need = need, free = free);
                return Err((EIO, Some(vm)));
            }
            None => {
                // The chain's lock was not to be had: nothing was written.
                klog::event!(Warn, "gx", "hw.chain_busy_at_submit", ring = ty.name());
                return Err((EIO, Some(vm)));
            }
            other => {
                // A ring, doorbell or memory write that did not land: nothing can be said about the engine. The words and the
                // write pointer's shadow come before the doorbell, and SDMA polls that shadow, so the frame may run: what it
                // reaches stays until the engines are halted.
                klog::event!(Error, "gx", "hw.submit_failed", ring = ty.name(), why = format!("{other:?}").as_str(); "the device counts as lost");
                self.shared.lost.store(true, Ordering::Release);
                self.grave.push(OnRing { seq, hold: hold.clone(), member, vm: Some(vm) });
                return Err((EIO, None));
            }
        };
        let meta = JobMeta { owner: Owner::Client { ctx: j.entity.ctx_id(), entity: j.entity.id() }, vmid, pasid: vm.pasid };
        match self.domain.submit(rid, meta, start, end, now) {
            Ok(s) if s == seq => {}
            other => {
                // The frame's fence has a number the watch does not know: it would call a healthy ring garbage.
                klog::event!(Error, "gx", "hw.watch_refused", ring = ty.name(), why = format!("{other:?}").as_str(); "the device counts as lost");
                self.shared.lost.store(true, Ordering::Release);
            }
        }
        if vm.needs_flush {
            self.last_flush[vmid as usize & 0xf] = Some((ri, seq));
        }
        self.rings[ri].current_ctx = Some(fctx);
        klog::event!(Debug, "gx", "hw.job_on_ring", ring = ty.name(), seq = seq, vmid = vmid, flush = vm.needs_flush, sync = sync.is_some(), start = start, end = end);
        self.rings[ri].jobs.push_back(OnRing { seq, hold: hold.clone(), member, vm: Some(vm) });
        Ok(())
    }
}

