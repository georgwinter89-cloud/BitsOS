//! DR-8: what a command submission hands the backend (`GpuDevice::submit`): a gang of jobs on one client VM,
//! the fences it waits for, the fences it signals, the user fence and what it keeps alive. The types carry no
//! detail of a backend: the fake runs the IBs in its PM4/SDMA interpreter, GX-8 puts them on the rings.
//!
//! Linux: `struct amdgpu_job` (amdgpu_job.h), the scheduler entity of a context (`amdgpu_ctx_entity`,
//! `drm_sched_entity`) and its fences (`drm_sched_fence`: scheduled and finished).

use crate::dma_buf::Exporter;
use crate::fence::Fence;
use crate::gem::Bo;
use crate::gpu::GpuVm;
use crate::uapi::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use vfs::Errno;

/// `enum drm_sched_priority` as `amdgpu_ctx_to_drm_sched_prio` (amdgpu_ctx.c:67) maps the context priority.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SchedPriority {
    Min,
    Normal,
    High,
}

impl SchedPriority {
    /// The scheduler priority of a context priority (UNSET and unknown values were NORMAL already).
    pub fn of_ctx(p: i32) -> SchedPriority {
        if p < 0 {
            SchedPriority::Min
        } else if p as u32 >= AMDGPU_CTX_PRIORITY_HIGH {
            SchedPriority::High
        } else {
            SchedPriority::Normal
        }
    }
}

static ENTITY_IDS: AtomicU64 = AtomicU64::new(1);

/// A context's view of the device's reset counter (`ctx->reset_counter`, `ctx->reset_counter_query`), shared with its
/// entities so a backend can blame the context from a job.
#[derive(Default)]
pub struct ResetSnaps {
    pub reset_counter: AtomicU32,
    pub reset_counter_query: AtomicU32,
}

impl ResetSnaps {
    pub fn new(now: u32) -> ResetSnaps {
        ResetSnaps { reset_counter: AtomicU32::new(now), reset_counter_query: AtomicU32::new(now) }
    }
    /// GR-3, `amdgpu_fmt::told::Counters::blame`: Mesa reads GUILTY only inside RESET and a queue-scope recovery does not
    /// move the device's counter (`now`), so the context's snapshots leave it. Off for good as after a real move; the old
    /// query answers UNKNOWN_RESET once.
    pub fn leave(&self, now: u32) {
        for snap in [&self.reset_counter, &self.reset_counter_query] {
            let _ = snap.compare_exchange(now, now.wrapping_sub(1), Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

/// A context's queue on one HW IP ring (`amdgpu_ctx_entity`): its jobs run in submission order.
pub struct Entity {
    id: u64,
    pub ip_type: u32,
    pub ring: u32,
    pub priority: SchedPriority,
    /// The context's guilty flag (`drm_sched_entity.guilty` = `&ctx->guilty`): the backend sets it when a job of
    /// the entity hangs; jobs of a guilty entity are not run (ECANCELED).
    pub guilty: Arc<AtomicBool>,
    snaps: Arc<ResetSnaps>,
    /// `drm_sched_entity_kill` (the context ended): jobs that have not started finish with this errno instead.
    killed: AtomicI64,
    /// Fence contexts: `scheduled` and `scheduled + 1` (finished), as `drm_sched_entity_init` takes two.
    pub fence_context: u64,
}

impl Entity {
    pub fn new(ip_type: u32, ring: u32, priority: SchedPriority, guilty: Arc<AtomicBool>, snaps: Arc<ResetSnaps>) -> Arc<Entity> {
        Arc::new(Entity {
            id: ENTITY_IDS.fetch_add(1, Ordering::Relaxed),
            ip_type,
            ring,
            priority,
            guilty,
            snaps,
            killed: AtomicI64::new(0),
            fence_context: crate::fence::context_alloc(2),
        })
    }
    /// Unique over the kernel's run: jobs of one entity run in order.
    pub fn id(&self) -> u64 {
        self.id
    }
    /// Names the context: its entities share the guilty flag.
    pub fn ctx_id(&self) -> u64 {
        Arc::as_ptr(&self.guilty) as usize as u64
    }
    /// GR-3, `Action::Blame` of a queue-scope recovery, as `amdgpu_fmt::told::Counters::blame`: a job of this entity
    /// hung or stopped. Mesa reads GUILTY only inside RESET and such a recovery does not move the device's counter, so
    /// the context's own snapshots leave it (`now`: that counter). Off for good as after a real move; the old query
    /// answers UNKNOWN_RESET once.
    pub fn blame(&self, now: u32) {
        self.guilty.store(true, Ordering::Release);
        self.snaps.leave(now);
    }
    /// The errno a job that has not started yet ends with (None: run it).
    pub fn refused(&self) -> Option<Errno> {
        let k = self.killed.load(Ordering::Acquire);
        if k != 0 {
            Some(k)
        } else if self.guilty.load(Ordering::Acquire) {
            Some(linux::ab::errno_more::ECANCELED)
        } else {
            None
        }
    }
    pub(crate) fn kill(&self, errno: Errno) {
        let _ = self.killed.compare_exchange(0, errno, Ordering::AcqRel, Ordering::Acquire);
    }
}

/// One IB: `len_dw` dwords at `va` (48-bit index space, the canonical hole taken out) with the uAPI's
/// `AMDGPU_IB_FLAG_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobIb {
    pub va: u64,
    pub len_dw: u32,
    pub flags: u32,
}

/// CP_GFX_SHADOW (`amdgpu_cs_p2_shadow`): where the CP keeps its register shadow, CSA and GDS backup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GfxShadow {
    pub shadow_va: u64,
    pub csa_va: u64,
    pub gds_va: u64,
    pub init: bool,
}

/// One member of a gang (`amdgpu_job`).
pub struct Job {
    pub entity: Arc<Entity>,
    pub ibs: Vec<JobIb>,
    pub shadow: Option<GfxShadow>,
    /// Signals when the job starts on the hardware (`s_fence->scheduled`).
    pub scheduled: Arc<Fence>,
    /// Signals when it ended (`s_fence->finished`), with an errno when it did not run to its end.
    pub finished: Arc<Fence>,
}

/// The user fence (`AMDGPU_CHUNK_ID_FENCE`): the leader writes `value` as 64 bits at `offset` of the object
/// when its IBs end, before its `finished` signals (`amdgpu_ib_schedule` -> `emit_fence` 64-bit).
pub struct UserFence {
    pub mem: Arc<dyn Exporter>,
    pub offset: u64,
    pub value: u64,
}

/// One command submission (`amdgpu_cs_parser` at `amdgpu_cs_submit`): jobs on distinct entities, the leader
/// last. Contract for a backend:
/// - every job waits for `deps`, the leader also for the other members' `scheduled`; a job of an entity runs
///   after the earlier jobs of that entity; between entities a higher priority goes first;
/// - before a job runs: `entity.refused()` or a `generation` older than the device's VRAM-lost counter ends it
///   unrun (scheduled, then finished with that errno, ECANCELED for the generation);
/// - a job that hangs or times out ends with ETIME and sets its entity's `guilty` (Linux's recovery);
/// - each job signals `scheduled` once and `finished` once, always, in a bounded time once `deps` signaled
///   (B4); the backend keeps `vm`, `bos` and the user fence's memory until every `finished` signaled.
pub struct Submission {
    pub jobs: Vec<Job>,
    pub vm: Arc<dyn GpuVm>,
    pub deps: Vec<Arc<Fence>>,
    pub user_fence: Option<UserFence>,
    /// The BO list and the user fence object.
    pub bos: Vec<Arc<Bo>>,
    /// `amdgpu_vm_generation` when the CS was made.
    pub generation: u32,
}

impl Submission {
    pub fn leader(&self) -> Option<&Job> {
        self.jobs.last()
    }
    /// Ends every job unrun with `errno` (a backend that cannot take the submission, a dropped device).
    pub fn cancel(&self, errno: Errno) {
        for j in &self.jobs {
            j.scheduled.signal();
            j.finished.signal_error(errno as i32);
        }
    }
}
