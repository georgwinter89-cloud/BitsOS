//! DR-4: `DRM_IOCTL_AMDGPU_CTX` (Linux `amdgpu_ctx_ioctl`, linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_ctx.c:664):
//! GPU contexts per open file (ids from 1, lowest free first, no fixed count), their priority, reset and VRAM-lost
//! queries and the device's stable pstate. Jobs and scheduler entities come with CS.

use crate::device::Device;
use crate::dispatch::Ctx;
use crate::file::DrmFile;
use crate::gpu::GpuDevice;
use crate::uapi::*;
use crate::entity::{self, CtxEntity};
use crate::wire::Wire;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use linux::errno::*;
use vfs::Errno;

/// Context ids across all files (the stable pstate owner is named by it).
static UIDS: AtomicU64 = AtomicU64::new(1);

/// One GPU context (`struct amdgpu_ctx`).
pub struct GpuCtx {
    uid: u64,
    priority: i32,
    /// `reset_counter` at creation and at the last QUERY_STATE; [`GpuCtx::blame`] moves both off the device's.
    reset_counter: AtomicU32,
    reset_counter_query: AtomicU32,
    /// `amdgpu_vm_generation` at creation (the VRAM-lost counter; one VM per file comes with GEM_VA).
    generation: u32,
    /// Shared with its entities' jobs: the backend sets it when one of them hangs (`ctx->guilty`).
    guilty: Arc<AtomicBool>,
    /// DR-8: the scheduler entities by (HW IP, ring), made at first use (`ctx->entities`).
    entities: SpinLock<BTreeMap<(u32, u32), Arc<CtxEntity>>>,
    /// The pstate the device returns to when the context ends.
    stable_pstate: u32,
    /// Set by `fini`; a pstate change still running on another thread then gives the pstate back itself.
    ended: AtomicBool,
}

impl GpuCtx {
    /// GR-3, `Action::Blame` of a queue-scope recovery, as `amdgpu_fmt::told::Counters::blame`: Mesa reads GUILTY only
    /// inside RESET and such a recovery does not move the device's counter, so the context's own snapshots leave it.
    /// Off for good as after a real move; the old query answers UNKNOWN_RESET once.
    pub fn blame(&self, gpu: &dyn GpuDevice) {
        self.guilty.store(true, Ordering::Release);
        let now = gpu.reset_counter();
        for snap in [&self.reset_counter, &self.reset_counter_query] {
            let _ = snap.compare_exchange(now, now.wrapping_sub(1), Ordering::AcqRel, Ordering::Acquire);
        }
    }
    pub fn priority(&self) -> i32 {
        self.priority
    }
    pub fn generation(&self) -> u32 {
        self.generation
    }
    pub fn guilty(&self) -> bool {
        self.guilty.load(Ordering::Acquire)
    }

    /// `amdgpu_ctx_get_entity` (amdgpu_ctx.c:433): EINVAL for an IP, instance or ring outside the table and, as
    /// on CachyOS's kernel, for an IP without a ready ring (6.6 made an entity without a run queue there); the
    /// entity's error once a job of it failed (`drm_sched_entity_error`).
    pub fn entity(&self, gpu: &dyn GpuDevice, ip_type: u32, instance: u32, ring: u32) -> Result<Arc<CtxEntity>, Errno> {
        entity::check(ip_type, instance, ring)?;
        if gpu.rings_ready(ip_type) == 0 {
            return Err(EINVAL);
        }
        let e = {
            let mut m = self.entities.lock();
            match m.get(&(ip_type, ring)) {
                Some(e) => e.clone(),
                None => {
                    let e = Arc::new(CtxEntity::new(ip_type, ring, self.priority, self.guilty.clone()));
                    m.insert((ip_type, ring), e.clone());
                    e
                }
            }
        };
        if let Some(err) = e.error() {
            return Err(err);
        }
        Ok(e)
    }

    fn all_entities(&self) -> Vec<Arc<CtxEntity>> {
        self.entities.lock().values().cloned().collect()
    }
}

/// `amdgpu_ctx_priority_is_valid`: the five named levels; anything else (UNSET included) counts as NORMAL.
fn valid_priority(p: i32) -> bool {
    matches!(p, AMDGPU_CTX_PRIORITY_VERY_LOW | AMDGPU_CTX_PRIORITY_LOW) || matches!(p as u32, AMDGPU_CTX_PRIORITY_NORMAL | AMDGPU_CTX_PRIORITY_HIGH | AMDGPU_CTX_PRIORITY_VERY_HIGH)
}

/// `amdgpu_ctx_priority_permit` (amdgpu_ctx.c:99): above NORMAL needs CAP_SYS_NICE or the DRM master.
fn permit(ctx: &Ctx, p: i32) -> Result<(), Errno> {
    if p <= AMDGPU_CTX_PRIORITY_NORMAL as i32 || ctx.caller.sys_nice || ctx.file.is_master() {
        Ok(())
    } else {
        Err(EACCES)
    }
}

pub fn ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let Some(args) = drm_amdgpu_ctx::decode(data) else { return Err(EINVAL) };
    let a = args.r#in();
    let priority = if valid_priority(a.priority) { a.priority } else { AMDGPU_CTX_PRIORITY_NORMAL as i32 };
    let r = (|| match a.op {
        AMDGPU_CTX_OP_ALLOC_CTX => {
            let (r, id) = match alloc(ctx, gpu, priority) {
                Ok(id) => (Ok(0), id),
                Err(e) => (Err(e), 0),
            };
            // out.alloc.ctx_id over in.op; the rest of the bytes stay as they came.
            data[0..4].copy_from_slice(&id.to_le_bytes());
            r
        }
        AMDGPU_CTX_OP_FREE_CTX => {
            let c = ctx.file.gpu_ctxs.lock().remove(a.ctx_id).ok_or(EINVAL)?;
            fini(ctx.dev, gpu, &c, false, end_wait());
            Ok(0)
        }
        AMDGPU_CTX_OP_QUERY_STATE => {
            let c = lookup(ctx.file, a.ctx_id)?;
            let now = gpu.reset_counter();
            let before = c.reset_counter_query.swap(now, Ordering::AcqRel);
            let status = if before == now { AMDGPU_CTX_NO_RESET } else { AMDGPU_CTX_UNKNOWN_RESET };
            state_out(data, 0, status);
            Ok(0)
        }
        AMDGPU_CTX_OP_QUERY_STATE2 => {
            let c = lookup(ctx.file, a.ctx_id)?;
            let mut flags = 0u64;
            if c.reset_counter.load(Ordering::Acquire) != gpu.reset_counter() {
                flags |= AMDGPU_CTX_QUERY2_FLAGS_RESET as u64;
            }
            if c.generation != gpu.vram_lost_counter() {
                flags |= AMDGPU_CTX_QUERY2_FLAGS_VRAMLOST as u64;
            }
            if c.guilty.load(Ordering::Acquire) {
                flags |= AMDGPU_CTX_QUERY2_FLAGS_GUILTY as u64;
            }
            if gpu.in_reset() {
                flags |= AMDGPU_CTX_QUERY2_FLAGS_RESET_IN_PROGRESS as u64;
            }
            // The RAS counters only exist on chips with RAS (ras_enabled); the 780M has none.
            state_out(data, flags, 0);
            Ok(0)
        }
        AMDGPU_CTX_OP_GET_STABLE_PSTATE => {
            if a.flags != 0 {
                return Err(EINVAL);
            }
            lookup(ctx.file, a.ctx_id)?;
            data[0..4].copy_from_slice(&gpu.stable_pstate().to_le_bytes());
            Ok(0)
        }
        AMDGPU_CTX_OP_SET_STABLE_PSTATE => {
            if a.flags & !AMDGPU_CTX_STABLE_PSTATE_FLAGS_MASK != 0 {
                return Err(EINVAL);
            }
            let want = a.flags & AMDGPU_CTX_STABLE_PSTATE_FLAGS_MASK;
            if want > AMDGPU_CTX_STABLE_PSTATE_PEAK {
                return Err(EINVAL);
            }
            let c = lookup(ctx.file, a.ctx_id)?;
            set_stable_pstate(gpu, &c, want, false)
        }
        op => {
            ctx.dev.note_unknown("ctx.unknown_op", op);
            Err(EINVAL)
        }
    })();
    if let Err(e) = r {
        klog::event!(Debug, "drm", "ctx.refused", minor = ctx.dev.render_minor(), op = a.op, ctx = a.ctx_id, errno = e);
    }
    r
}

/// `union drm_amdgpu_ctx_out.state`: flags, hangs (always 0), reset status.
fn state_out(data: &mut [u8], flags: u64, reset_status: u32) {
    data[0..8].copy_from_slice(&flags.to_le_bytes());
    data[8..12].copy_from_slice(&0u32.to_le_bytes());
    data[12..16].copy_from_slice(&reset_status.to_le_bytes());
}

pub(crate) fn lookup(file: &DrmFile, id: u32) -> Result<Arc<GpuCtx>, Errno> {
    file.gpu_ctxs.lock().get(id).cloned().ok_or(EINVAL)
}

/// `amdgpu_ctx_alloc` + `amdgpu_ctx_init`: the priority check comes before the id is taken for good (Linux takes
/// and returns it; either way the caller sees id 0 and the lowest free id stays free).
fn alloc(ctx: &Ctx, gpu: &dyn GpuDevice, priority: i32) -> Result<u32, Errno> {
    permit(ctx, priority)?;
    let owner = gpu.shared().pstate.lock().owner;
    let c = Arc::new(GpuCtx {
        uid: UIDS.fetch_add(1, Ordering::Relaxed),
        priority,
        reset_counter: AtomicU32::new(gpu.reset_counter()),
        reset_counter_query: AtomicU32::new(gpu.reset_counter()),
        generation: gpu.vram_lost_counter(),
        guilty: Arc::new(AtomicBool::new(false)),
        entities: SpinLock::named("drm.ctx.entities", BTreeMap::new()),
        stable_pstate: owner.map(|(_, p)| p).unwrap_or_else(|| gpu.stable_pstate()),
        ended: AtomicBool::new(false),
    });
    let id = ctx.file.gpu_ctxs.lock().insert(c)?;
    klog::event!(Debug, "drm", "ctx.alloc", minor = ctx.dev.render_minor(), client = ctx.file.client_id(), ctx = id, priority = priority);
    Ok(id)
}

/// `amdgpu_ctx_set_stable_pstate` (amdgpu_ctx.c:356): one context at a time owns a forced pstate (EBUSY for
/// another); NONE gives it back. Linux holds a mutex across the SMU call; here the lock only marks the change
/// (`changing`) and commits it, so the SMU wait never runs under a spinlock (B4). A second change while one runs
/// gets EBUSY instead of waiting. The owner changes only when the hardware took the new pstate, except at `fini`
/// (`ending`): the context is gone then, so it lets go even when the release failed.
fn set_stable_pstate(gpu: &dyn GpuDevice, c: &GpuCtx, want: u32, ending: bool) -> Result<i64, Errno> {
    {
        let mut st = gpu.shared().pstate.lock();
        if st.owner.is_some_and(|(uid, _)| uid != c.uid) || st.changing.is_some() {
            return Err(EBUSY);
        }
        if want == gpu.stable_pstate() {
            return Ok(0);
        }
        st.changing = Some(c.uid);
    }
    let r = gpu.force_stable_pstate(want);
    let owns = {
        let mut st = gpu.shared().pstate.lock();
        st.changing = None;
        if r.is_ok() {
            st.owner = if want == AMDGPU_CTX_STABLE_PSTATE_NONE { None } else { Some((c.uid, c.stable_pstate)) };
        } else if ending {
            st.owner = None;
        }
        st.owner.is_some_and(|(uid, _)| uid == c.uid)
    };
    match r {
        Ok(()) => klog::event!(Info, "drm", "ctx.stable_pstate", pstate = want),
        Err(e) => klog::event!(Warn, "drm", "ctx.stable_pstate_failed", pstate = want, ending = ending, errno = e),
    }
    // FREE ran while this change was on the hardware (its `fini` saw `changing` and got EBUSY): give it back now.
    if !ending && owns && c.ended.load(Ordering::SeqCst) {
        let _ = set_stable_pstate(gpu, c, c.stable_pstate, true);
    }
    r.map(|()| 0)
}

/// The deadline of one end wait (`MAX_WAIT_SCHED_ENTITY_Q_EMPTY` from now).
fn end_wait() -> u64 {
    crate::fence::now().unwrap_or(0).saturating_add(entity::END_WAIT_NS)
}

/// `amdgpu_ctx_fini`: its jobs get until `end` to start (the last close: to end), the rest end unrun
/// (`drm_sched_entity_destroy`); the pstate the context found comes back (while the device is still there).
fn fini(dev: &Device, gpu: &dyn GpuDevice, c: &GpuCtx, last_close: bool, end: u64) {
    c.ended.store(true, Ordering::SeqCst);
    entity::drain(&c.all_entities(), end, last_close);
    if let Some(_inside) = dev.enter() {
        let _ = set_stable_pstate(gpu, c, c.stable_pstate, true);
    }
}

/// The last close of a file (`amdgpu_ctx_mgr_fini` from postclose): every context ends. One deadline for all
/// of them, as `amdgpu_ctx_mgr_entity_flush` hands each entity what is left of one timeout: a file with many
/// contexts with stuck jobs still closes within a second.
pub fn release_file(dev: &Device, file: &DrmFile) -> usize {
    let all = file.gpu_ctxs.lock().drain();
    if let Some(gpu) = dev.driver().gpu() {
        let end = end_wait();
        for (_, c) in &all {
            fini(dev, gpu, c, true, end);
        }
    }
    all.len()
}
