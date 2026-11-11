//! DR-8: a context's scheduler entities (`amdgpu_ctx_entity`, linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_ctx.c):
//! one per HW IP ring a program names, made at first use, each with its sequence numbers and the fences of its
//! recent jobs (`amdgpu_ctx_add_fence`/`get_fence`, :748/:775), and the bounded waits CS, WAIT_CS and the end of
//! a context use.
//!
//! Linux keeps `amdgpu_sched_jobs` (32) fence slots per entity and makes the 33rd CS wait for the oldest job
//! (`amdgpu_ctx_wait_prev_fence`). Here a CS never waits (R1): the window keeps the last 32 sequence numbers as
//! Linux reports them, and older entries for as long as their job has not finished, so a wait on an old number
//! still sees the truth. What a program can query is the same: numbers older than the window are done.

use crate::fence::{self, Fence, Waiter};
use crate::job::{Entity, ResetSnaps, SchedPriority};
use crate::uapi::*;
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::AtomicBool;
use ksync::SpinLock;
use linux::errno::*;
use vfs::Errno;

/// `amdgpu_sched_jobs` (amdgpu_drv.c:147): how many recent sequence numbers an entity reports.
pub const WINDOW: u64 = 32;
/// `amdgpu_ctx_num_entities` (amdgpu_ctx.c:35; VPE as in CachyOS's kernel): entities per HW IP.
pub const NUM_ENTITIES: [u32; AMDGPU_HW_IP_NUM as usize] = [1, 4, 2, 1, 1, 1, 1, 1, 1, 1];
/// `MAX_WAIT_SCHED_ENTITY_Q_EMPTY` (gpu_scheduler.h:33): how long the end of a context waits for its jobs.
pub const END_WAIT_NS: u64 = 1_000_000_000;
/// How often a blocking wait looks at kill, signals and the device without a wake (B4).
const SLICE_NS: u64 = vfs::poll::KILL_SLICE_NS;

/// The scheduler's ring names (`drm_sched_fence_get_timeline_name` gives the ring's name).
const TIMELINES: [&str; AMDGPU_HW_IP_NUM as usize] =
    ["gfx_0.0.0", "comp_1.0.0", "sdma0", "uvd", "vce", "uvd_enc", "vcn_dec", "vcn_unified_0", "jpeg_dec", "vpe"];

/// A job's two fences.
#[derive(Clone)]
pub struct Slot {
    pub scheduled: Arc<Fence>,
    pub finished: Arc<Fence>,
}

struct Window {
    /// The next sequence number (`centity->sequence`, from 1).
    sequence: u64,
    /// The sequence number of `slots[0]`.
    base: u64,
    slots: VecDeque<Slot>,
    /// Jobs this entity ran as a gang member under another leader (no sequence number: Linux adds only the
    /// leader's fence to the context), until they finished.
    members: VecDeque<Slot>,
    /// The fences' seqno (`entity->fence_seq`: every job of the entity, leader or member).
    fence_seq: u64,
}

/// One entity of a context.
pub struct CtxEntity {
    pub job: Arc<Entity>,
    /// Held from the sequence number to the backend's queue: one entity's jobs reach it in order.
    pub(crate) submit: ksync::Mutex<()>,
    win: SpinLock<Window>,
}

impl CtxEntity {
    pub(crate) fn new(ip_type: u32, ring: u32, priority: i32, guilty: Arc<AtomicBool>, snaps: Arc<ResetSnaps>) -> CtxEntity {
        CtxEntity {
            job: Entity::new(ip_type, ring, SchedPriority::of_ctx(priority), guilty, snaps),
            submit: ksync::Mutex::named("drm.cs.submit", ()),
            win: SpinLock::named("drm.cs.window", Window { sequence: 1, base: 1, slots: VecDeque::new(), members: VecDeque::new(), fence_seq: 0 }),
        }
    }

    /// The fences of the next job (`drm_sched_job_arm`) and, for a leader, its sequence number
    /// (`amdgpu_ctx_add_fence`; 0 for a gang member); the caller holds `submit`. Finished jobs older than the
    /// window leave it.
    pub(crate) fn push(&self, leader: bool) -> Result<(u64, Slot), Errno> {
        let c = self.job.fence_context;
        let timeline = TIMELINES.get(self.job.ip_type as usize).copied().unwrap_or("amdgpu");
        let mut w = self.win.lock();
        while w.slots.len() as u64 >= WINDOW && w.slots.front().is_some_and(|s| fence::is_signaled(&s.finished)) {
            w.slots.pop_front();
            w.base += 1;
        }
        w.members.retain(|s| !fence::is_signaled(&s.finished));
        w.slots.try_reserve(1).map_err(|_| ENOMEM)?;
        w.members.try_reserve(1).map_err(|_| ENOMEM)?;
        w.fence_seq += 1;
        let n = w.fence_seq;
        let s = Slot { scheduled: Fence::new(c, n, "drm_sched", timeline), finished: Fence::new(c + 1, n, "drm_sched", timeline) };
        if !leader {
            w.members.push_back(s.clone());
            return Ok((0, s));
        }
        let seq = w.sequence;
        w.slots.push_back(s.clone());
        w.sequence += 1;
        Ok((seq, s))
    }

    /// Takes back the last `push` (the backend refused the job); the caller still holds `submit`.
    pub(crate) fn unpush(&self, seq: u64) {
        let mut w = self.win.lock();
        w.fence_seq -= 1;
        if seq == 0 {
            w.members.pop_back();
        } else if w.sequence == seq + 1 && w.slots.pop_back().is_some() {
            w.sequence = seq;
        }
    }

    /// `amdgpu_ctx_get_fence`: `u64::MAX` is the last job; EINVAL for a number not handed out yet; None when the
    /// job is known to be done (older than the window, or number 0).
    pub fn fence(&self, seq: u64) -> Result<Option<Slot>, Errno> {
        let w = self.win.lock();
        let seq = if seq == u64::MAX { w.sequence - 1 } else { seq };
        if seq >= w.sequence {
            return Err(EINVAL);
        }
        if seq < w.base {
            return Ok(None);
        }
        let s = w.slots.get((seq - w.base) as usize).cloned();
        // Linux drops numbers older than the window (they are done there); a job still running keeps its slot here.
        Ok(s.filter(|s| seq + WINDOW >= w.sequence || !fence::is_signaled(&s.finished)))
    }

    /// `drm_sched_entity_error`: the error of the last job that started (`entity->last_scheduled`).
    pub fn error(&self) -> Option<Errno> {
        let w = self.win.lock();
        let started = |q: &VecDeque<Slot>| q.iter().rev().find(|s| fence::is_signaled(&s.scheduled)).cloned();
        let s = match (started(&w.slots), started(&w.members)) {
            (Some(a), Some(b)) => if b.finished.seqno() > a.finished.seqno() { b } else { a },
            (a, b) => a.or(b)?,
        };
        let st = fence::status(&s.finished);
        (st < 0).then_some(-(st as Errno))
    }

    /// The fences of the entity's last job (leader or member).
    pub fn last(&self) -> Option<Slot> {
        let w = self.win.lock();
        match (w.slots.back(), w.members.back()) {
            (Some(a), Some(b)) => Some(if b.finished.seqno() > a.finished.seqno() { b.clone() } else { a.clone() }),
            (a, b) => a.or(b).cloned(),
        }
    }
}

/// The argument checks of `amdgpu_ctx_get_entity` (amdgpu_ctx.c:433).
pub fn check(ip_type: u32, instance: u32, ring: u32) -> Result<(), Errno> {
    let Some(&n) = NUM_ENTITIES.get(ip_type as usize) else { return Err(EINVAL) };
    if instance != 0 || ring >= n {
        return Err(EINVAL);
    }
    Ok(())
}

/// Waits for `f` until the deadline `end` (None: no deadline, in slices; Some(past): one look). Ok(true):
/// signaled; Ok(false): the deadline came first; Err from `stop` (kill, signal, unplug).
pub fn wait(f: &Arc<Fence>, end: Option<u64>, stop: &dyn Fn() -> Option<Errno>) -> Result<bool, Errno> {
    wait_any(core::slice::from_ref(f), end, stop).map(|i| i.is_some())
}

/// Waits for the first of `fs` to signal: its index, None at the deadline.
pub fn wait_any(fs: &[Arc<Fence>], end: Option<u64>, stop: &dyn Fn() -> Option<Errno>) -> Result<Option<usize>, Errno> {
    if fs.is_empty() {
        return Err(EINVAL);
    }
    if let Some(i) = fs.iter().position(fence::is_signaled) {
        return Ok(Some(i));
    }
    let w = Waiter::new();
    let mut regs = alloc::vec::Vec::new();
    regs.try_reserve_exact(fs.len()).map_err(|_| ENOMEM)?;
    for f in fs {
        regs.push(fence::add_callback(f, w.clone()));
    }
    loop {
        if let Some(i) = fs.iter().position(fence::is_signaled) {
            return Ok(Some(i));
        }
        if let Some(e) = stop() {
            return Err(e);
        }
        let nap = match end {
            None => SLICE_NS,
            Some(e) => match fence::now() {
                Some(now) if now < e => (e - now).min(SLICE_NS),
                _ => return Ok(None),
            },
        };
        w.sleep(nap);
    }
}

/// The end of a context (`drm_sched_entity_flush` + `_fini`, and for the last close `amdgpu_vm_wait_idle`):
/// waits until `end` (one deadline for every entity, and for the last close for every context of the file)
/// for its jobs to start (`finish`: also to end), then jobs not started yet end unrun with ESRCH. As
/// `drm_sched_entity_kill_jobs_cb` (sched_entity.c:174) they do that once their dependencies signaled, so a
/// reader of their fences never runs before what the job would have waited for; every fence signals in
/// bounded time (jobs are bounded, a dropped device ends its queue), so the job and what it holds go then.
/// Returns the jobs not started at `end`.
pub fn drain(es: &[Arc<CtxEntity>], end: u64, finish: bool) -> usize {
    let none = || None;
    let mut left = 0;
    for e in es {
        if let Some(s) = e.last() {
            let _ = wait(&s.scheduled, Some(end), &none);
            if finish {
                let _ = wait(&s.finished, Some(end), &none);
            }
            if !fence::is_signaled(&s.scheduled) {
                left += 1;
            }
        }
        e.job.kill(ESRCH);
    }
    if left > 0 {
        klog::event!(Info, "drm", "cs.entity_killed", entities = left; "jobs still waiting at the end of their context end with ESRCH");
    }
    left
}
