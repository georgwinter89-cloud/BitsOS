//! GX-8v: programs' video jobs on VCN 0's unified ring (`AMDGPU_HW_IP_VCN_ENC`; VCN 4 has no
//! decode-only ring). One `amdgpu-vcn` thread (Normal) is the ring's only writer; `submit` only queues. A job's
//! frame is `amdgpu_ib_schedule`'s for this ring: the in-ring flush of its VMID on MMHUB (`vcnvm::flush`, always:
//! the VMID manager's flush record is GFXHUB's), its PASID in IH_VMID_n_LUT_MM, the IBs at that VMID, FENCE + TRAP,
//! END (`vcnframe`); the commit pads to 64. Every frame is checked against MMHUB's ring list (`vcnvm::ring_ok`,
//! registers and values) before it is written. MMHUB's fault status is read each round (`vcnframe`).
//!
//! Linux's rules kept: no user fence on this ring (amdgpu_cs.c:373, `no_user_fence`: EINVAL); no TMZ; a job's
//! dependencies, then its entity's order; the video timeout (2 s, 7.3.1). A job that times out ends with ETIME and
//! its context guilty, the others on the ring ECANCELED, and so do the jobs still queued (they would never run); then
//! VCN is stopped and powered down and the ring takes no more jobs (ENODEV): Linux restarts the block
//! (vcn_v4_0_ring_reset: stop, start, the SRAM through the PSP), which needs the PSP on the job path, an open point. The thread polls the fence; the TRAP's interrupt is routed
//! (`ih6::Source::VcnTrap`) but not waited for.

use super::chain::Chain;
use super::vcnframe::{self, check_ibs, frame_words};
use super::vm::{HwVm, VmJob};
use crate::gpucore::gmc::{self, FaultRead, HubAddrs};
use crate::gpucore::vcn::{Layout, VcnAddrs};
use crate::gpucore::vcnring::{VcnQueue, mem, rd32};
use crate::gpucore::vcnvm::{self, RingList};
use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use drm::fence::{self, FenceCb, Registration, Waiter};
use drm::job::Submission;
use drm::uapi::*;
use ksync::{Mutex, SpinLock};
use linux::ab::errno_more::ECANCELED;
use linux::errno::{EINVAL, ENODEV, ENOMEM, ETIME};

use vfs::Errno;

/// What the VCN stage left for the job half.
#[derive(Clone, Copy, Debug)]
pub struct VcnUp {
    pub addrs: VcnAddrs,
    pub layout: Layout,
    /// The ring's write pointer after the stage (a counter; `VcnReport::wptr`).
    pub start: u64,
    pub doorbell: bool,
    pub mm: HubAddrs,
    pub smu: crate::ipmod::smu13::Addrs,
    pub timeout_us: u64,
}

struct Pend {
    sub: Submission,
    _regs: Vec<Registration>,
}

struct OnVcn {
    seq: u64,
    sub: Submission,
    vm: Option<VmJob>,
}

struct Inner {
    q: VcnQueue,
    seq: u64,
    on: VecDeque<OnVcn>,
    /// The fence and read pointer last seen, and when they last moved.
    seen: (Option<u32>, u64),
    since_us: u64,
    /// Jobs a hang left: what they reach stays until VCN is powered down (`stop`).
    grave: Vec<OnVcn>,
    /// The queue's head while it waits (dependencies, a VMID, ring room).
    head: Option<Pend>,
    /// MMHUB's fault status last seen (`vcnframe`).
    mm_fault: u32,
}

pub struct VcnJobs {
    chain: Arc<dyn Chain>,
    up: VcnUp,
    allowed: RingList,
    /// A hang ended the ring for this boot.
    pub lost: AtomicBool,
    closing: AtomicBool,
    pending: SpinLock<VecDeque<Pend>>,
    inner: Mutex<Inner>,
    wake: Arc<Waiter>,
    thread: SpinLock<Option<sched::JoinHandle>>,
    pub done: AtomicU64,
    pub hangs: AtomicU64,
}

impl VcnJobs {
    /// The job half on the ring the stage left; nothing is written here.
    pub(crate) fn new(chain: Arc<dyn Chain>, up: VcnUp) -> Result<Arc<VcnJobs>, String> {
        let q = VcnQueue::new(up.addrs, up.layout.ring, up.layout.wptr, up.start, up.doorbell).map_err(|e| format!("the VCN queue: {e:?}"))?;
        let (mut ok, mut fault0) = (false, 0);
        chain.with(&mut |o| {
            ok = mem(o.io(), up.layout.job_fence, &[0; 8]).is_ok();
            // A status already there (an earlier boot's, another client's) is nobody's job of ours.
            fault0 = gmc::read_fault(o.io(), &up.mm).status.unwrap_or(0);
        });
        if !ok {
            return Err(String::from("the VCN fence slot could not be written"));
        }
        let allowed = vcnvm::ring_writable(&up.mm, up.addrs.lut_mm);
        let j = Arc::new(VcnJobs {
            chain,
            up,
            allowed,
            lost: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            pending: SpinLock::named("amdgpu.hw.vcn_pending", VecDeque::new()),
            inner: Mutex::named("amdgpu.hw.vcn", Inner { q, seq: 0, on: VecDeque::new(), seen: (Some(0), up.start), since_us: 0, grave: Vec::new(), head: None, mm_fault: fault0 }),
            wake: Waiter::new(),
            thread: SpinLock::named("amdgpu.hw.vcn_thread", None),
            done: AtomicU64::new(0),
            hangs: AtomicU64::new(0),
        });
        #[cfg(not(test))]
        {
            let me = j.clone();
            let t = sched::spawn("amdgpu-vcn", sched::Class::Normal, move || me.thread_main()).map_err(|e| format!("the VCN thread: {e:?}"))?;
            *j.thread.lock() = Some(t);
        }
        klog::event!(Info, "gx", "hw.vcn_up", start = up.start, doorbell = up.doorbell);
        Ok(j)
    }

    /// Programs may submit: the ring is not lost.
    pub fn users(&self) -> bool {
        !self.lost.load(Ordering::Acquire)
    }

    /// MMHUB's fault registers now (reads only; `None`: the chain is busy).
    pub fn mm_fault(&self) -> Option<FaultRead> {
        let mut f = None;
        self.chain.with(&mut |o| f = Some(gmc::read_fault(o.io(), &self.up.mm)));
        f
    }

    /// A program's submission.
    pub fn submit(&self, sub: Submission) -> Result<(), Errno> {
        let refuse = |sub: Submission, e: Errno| {
            sub.cancel(e);
            Err(e)
        };
        if self.lost.load(Ordering::Acquire) || self.closing.load(Ordering::Acquire) {
            return refuse(sub, ENODEV);
        }
        // amdgpu_cs.c:373: this ring has no user fence; one job per CS (no gang member on VCN).
        let bad = sub.user_fence.is_some() || sub.jobs.len() != 1 || sub.jobs.iter().any(|j| j.entity.ip_type != AMDGPU_HW_IP_VCN_ENC || check_ibs(&j.ibs).is_err());
        if bad {
            klog::event!(Debug, "gx", "hw.vcn_refused", jobs = sub.jobs.len(); "a user fence, a gang, a secure or bad IB on VCN");
            return refuse(sub, EINVAL);
        }
        let mut regs = Vec::new();
        if regs.try_reserve_exact(sub.deps.len()).is_err() {
            return refuse(sub, ENOMEM);
        }
        let wake: Arc<dyn FenceCb> = self.wake.clone();
        for d in &sub.deps {
            if let Some(r) = fence::add_callback(d, wake.clone()) {
                regs.push(r);
            }
        }
        // The queue grows with its spin lock dropped (`ksync::Spare`, as `Jobs::queue`): no allocation under it.
        let mut spare = ksync::Spare::new();
        let p = Pend { sub, _regs: regs };
        loop {
            let mut q = self.pending.lock();
            if self.closing.load(Ordering::Acquire) {
                drop(q);
                return refuse(p.sub, ENODEV);
            }
            if spare.fits(&mut *q, 1) {
                q.push_back(p);
                return Ok(());
            }
            drop(q);
            if spare.grow().is_err() {
                return refuse(p.sub, ENOMEM);
            }
        }
    }

    pub fn kick(&self) {
        self.wake.call();
        #[cfg(test)]
        self.round();
    }

    #[cfg(not(test))]
    fn thread_main(&self) {
        let mut nap = 100_000u64;
        loop {
            let (busy, moved) = self.round();
            if !busy && self.closing.load(Ordering::Acquire) {
                break;
            }
            nap = if !busy { 1_000_000_000 } else if moved { 100_000 } else { nap.saturating_mul(2).clamp(100_000, 2_000_000) };
            self.wake.sleep(nap);
        }
        klog::event!(Debug, "gx", "hw.vcn_thread_end");
    }

    /// One look: jobs that ended, a hang, jobs that may start. (busy, something moved)
    pub(crate) fn round(&self) -> (bool, bool) {
        let mut ended: Vec<(OnVcn, Option<Errno>)> = Vec::new();
        // Fences signal in thread context with no lock of this half held (their callbacks wake readers).
        let mut late: Vec<(Arc<drm::fence::Fence>, i32)> = Vec::new();
        let mut moved = false;
        let mut g = self.inner.lock();
        let now = super::clock_ns() / 1000;
        let fence_mc = self.up.layout.job_fence;
        let mut fence = None;
        let mut rptr = g.seen.1;
        let mut fault = None;
        self.chain.with(&mut |o| {
            fence = rd32(o.io(), fence_mc);
            rptr = g.q.rptr(o.io()).unwrap_or(rptr);
            fault = Some(gmc::read_fault(o.io(), &self.up.mm));
        });
        if let Some((status, addr)) = fault.as_ref().and_then(|f| vcnframe::new_fault(f, g.mm_fault)) {
            g.mm_fault = status;
            let mine = vcnframe::give_fault(status, addr, g.on.iter().filter_map(|j| j.vm.as_ref().map(|v| (v.vmid, &j.sub))));
            klog::event!(Warn, "gx", "hw.vcn_mm_fault", status = klog::Hex(u64::from(status)), page = klog::Hex(addr), job = mine; "an MMHUB fault (not cleared: later ones only set MORE_FAULTS)");
        }
        if (fence, rptr) != g.seen {
            g.seen = (fence, rptr);
            g.since_us = now;
            moved = true;
        }
        if let Some(f) = fence {
            while g.on.front().is_some_and(|j| j.seq as u32 == f || (f.wrapping_sub(j.seq as u32) as i32) > 0) {
                if let Some(j) = g.on.pop_front() {
                    ended.push((j, None));
                }
            }
        }
        if !g.on.is_empty() && now.saturating_sub(g.since_us) >= self.up.timeout_us {
            self.hang(&mut g, &mut late);
        }
        // A lost ring runs nothing more: what still waits ends unrun (else its fences would wait for the boot's end).
        let mut unrun = Vec::new();
        if self.lost.load(Ordering::Acquire) {
            // Moved out whole: nothing allocates under the spin lock.
            let rest = core::mem::take(&mut *self.pending.lock());
            unrun.extend(g.head.take().into_iter().chain(rest));
        } else {
            self.start_ready(&mut g, &mut ended, &mut late, now);
        }
        let busy = !g.on.is_empty() || g.head.is_some() || !self.pending.lock().is_empty();
        drop(g);
        if !unrun.is_empty() {
            klog::event!(Warn, "gx", "hw.vcn_unrun", jobs = unrun.len(); "jobs queued behind a VCN hang end unrun");
        }
        for p in unrun {
            p.sub.cancel(ECANCELED);
        }
        for (f, e) in late {
            if e == 0 {
                f.signal();
            } else {
                f.signal_error(e);
            }
        }
        for (j, e) in ended {
            self.finish(j, e);
        }
        (busy, moved)
    }

    fn hang(&self, g: &mut Inner, late: &mut Vec<(Arc<drm::fence::Fence>, i32)>) {
        self.hangs.fetch_add(1, Ordering::AcqRel);
        self.lost.store(true, Ordering::Release);
        klog::event!(Error, "gx", "hw.vcn_hang", jobs = g.on.len(), timeout_us = self.up.timeout_us; "VCN did not finish a job: stopped and powered down, the ring takes no more jobs this boot");
        let mut first = true;
        while let Some(j) = g.on.pop_front() {
            if first {
                j.sub.jobs[0].entity.guilty.store(true, Ordering::Release);
            }
            late.push((j.sub.jobs[0].finished.clone(), if first { ETIME } else { ECANCELED } as i32));
            self.done.fetch_add(1, Ordering::Relaxed);
            // The memory stays held: the VCPU may read it until the power-down below proved it stopped.
            g.grave.push(j);
            first = false;
        }
        let (a, s) = (self.up.addrs, self.up.smu);
        let mut halted = false;
        self.chain.with(&mut |o| halted = crate::gpucore::vcn::halt(o.io(), &a, &s));
        klog::event!(Warn, "gx", "hw.vcn_halted", powered_down = halted);
    }

    fn start_ready(&self, g: &mut Inner, ended: &mut Vec<(OnVcn, Option<Errno>)>, late: &mut Vec<(Arc<drm::fence::Fence>, i32)>, now: u64) {
        loop {
            // The head waits in `head` (dependencies, a VMID, ring room): nothing goes back into the spin-locked queue.
            let p = match g.head.take() {
                Some(p) => p,
                None => match self.pending.lock().pop_front() {
                    Some(p) => p,
                    None => return,
                },
            };
            if !p.sub.deps.iter().all(fence::is_signaled) {
                g.head = Some(p);
                return;
            }
            let (sub, regs) = (p.sub, p._regs);
            if let Some(e) = sub.jobs[0].entity.refused() {
                ended.push((OnVcn { seq: 0, sub, vm: None }, Some(e)));
                continue;
            }
            let Some(vm) = sub.vm.as_any().and_then(|a| a.downcast_ref::<HwVm>()).and_then(HwVm::arc) else {
                ended.push((OnVcn { seq: 0, sub, vm: None }, Some(EINVAL)));
                continue;
            };
            let vj = match vm.begin_job() {
                Ok(Some(v)) => v,
                // Every VMID busy: try again next round.
                Ok(None) => {
                    g.head = Some(Pend { sub, _regs: regs });
                    return;
                }
                Err(e) => {
                    ended.push((OnVcn { seq: 0, sub, vm: None }, Some(e)));
                    continue;
                }
            };
            // The VMID manager records GFXHUB's flushes: this job flushes MMHUB itself and GFXHUB not at all.
            vj.forget_gfxhub_flush();
            let seq = g.seq + 1;
            let Some(words) = frame_words(&self.up, vj.vmid as u8, vj.pd, vj.pasid, &sub.jobs[0].ibs, seq).filter(|w| vcnvm::ring_ok(w, &self.allowed)) else {
                klog::event!(Error, "gx", "hw.vcn_frame_refused", vmid = vj.vmid; "a frame off MMHUB's ring list");
                vj.cancel();
                ended.push((OnVcn { seq: 0, sub, vm: None }, Some(EINVAL)));
                continue;
            };
            let mut res = Err(String::from("the chain is busy"));
            let n = words.len() as u64;
            self.chain.with(&mut |o| res = g.q.submit(o.io(), n, |r| r.write(&words)).map_err(|e| format!("{e:?}")));
            match res {
                Ok(_) => {
                    g.seq = seq;
                    if g.on.is_empty() {
                        g.since_us = now;
                    }
                    late.push((sub.jobs[0].scheduled.clone(), 0));
                    g.on.push_back(OnVcn { seq, sub, vm: Some(vj) });
                }
                Err(why) => {
                    vj.cancel();
                    // The ring is full or the chain busy: the job waits for the next round.
                    klog::event!(Debug, "gx", "hw.vcn_later", why = why.as_str());
                    g.head = Some(Pend { sub, _regs: regs });
                    return;
                }
            }
        }
    }

    fn finish(&self, j: OnVcn, e: Option<Errno>) {
        // A job ended unrun was never scheduled: once, before its end (the Submission contract).
        j.sub.jobs[0].scheduled.signal();
        let f = &j.sub.jobs[0].finished;
        match e {
            None => {
                f.signal();
            }
            Some(x) => {
                f.signal_error(x as i32);
            }
        }
        self.done.fetch_add(1, Ordering::Relaxed);
        drop(j.vm);
    }

    /// Ends the thread: queued jobs end unrun (ECANCELED), what is on the ring gets `close_us` to end.
    pub fn stop(&self, close_us: u64) {
        self.closing.store(true, Ordering::Release);
        let t0 = super::clock_ns() / 1000;
        while self.inner.lock().on.front().is_some() && super::clock_ns() / 1000 - t0 < close_us && !self.lost.load(Ordering::Acquire) {
            self.round();
            #[cfg(not(test))]
            sched::sleep_ns(1_000_000);
        }
        let head = self.inner.lock().head.take();
        // Moved out whole: nothing allocates under the spin lock.
        let rest = core::mem::take(&mut *self.pending.lock());
        let queued = head.into_iter().chain(rest);
        for p in queued {
            p.sub.cancel(ECANCELED);
        }
        self.wake.call();
        // The handle leaves its spin lock before the join sleeps (R5).
        let t = self.thread.lock().take();
        if let Some(t) = t
            && !t.join_timeout(close_us.saturating_mul(1000).saturating_add(2_000_000_000))
        {
            klog::event!(Error, "gx", "hw.vcn_thread_stuck");
        }
        let mut gone = Vec::new();
        let mut g = self.inner.lock();
        while let Some(j) = g.on.pop_front() {
            gone.push(j.sub.jobs[0].finished.clone());
            g.grave.push(j);
        }
        drop(g);
        for f in gone {
            f.signal_error(ECANCELED as i32);
        }
    }

    pub fn describe(&self) -> Vec<String> {
        let g = self.inner.lock();
        alloc::vec![format!(
            "backend VCN: ring at wptr {:#x}, {} on the ring, {} queued, {} done, {} hangs; programs {}",
            g.q.wptr(),
            g.on.len(),
            self.pending.lock().len(),
            self.done.load(Ordering::Relaxed),
            self.hangs.load(Ordering::Relaxed),
            if self.users() { "may submit" } else { "refused (the ring is lost)" }
        )]
    }
}

