//! DR-8: `DRM_IOCTL_AMDGPU_CS` (Linux `amdgpu_cs_ioctl`, linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_cs.c:1382):
//! the chunks Mesa sends (IB, FENCE, BO_HANDLES, DEPENDENCIES, SCHEDULED_DEPENDENCIES, SYNCOBJ_IN/OUT,
//! SYNCOBJ_TIMELINE_WAIT/SIGNAL, CP_GFX_SHADOW) with Linux's checks in Linux's order (parser_init, pass1, pass2,
//! the BO list, the VM, the implicit sync, submit), and the gang of jobs it hands the backend.
//!
//! Nothing a program can see changes before the backend took the jobs: the sequence number, the fences on the
//! objects and the out-syncobjs come after `GpuDevice::submit` answered Ok.

use crate::ctx::{self, GpuCtx};
use crate::bo_list::{bo_list_create, bo_list_entries, copy_in, rec};
use crate::dispatch::Ctx;
use crate::entity::CtxEntity;
use crate::fence::Fence;
use crate::gem::Bo;
use crate::gpu::GpuDevice;
use crate::job::{GfxShadow, Job, JobIb, Submission, UserFence};
use crate::resv::Usage;
use crate::syncobj::{self, Syncobj};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::sync::Arc;
use alloc::vec::Vec;
use linux::errno::*;
use linux::ab::errno_more::ECANCELED;
use vfs::Errno;

/// `AMDGPU_CS_GANG_SIZE` (amdgpu_cs.h:37): entities one CS may feed.
pub const GANG_SIZE: usize = 4;
/// The canonical hole of a GPU VA (`AMDGPU_GMC_HOLE_MASK`).
const HOLE_MASK: u64 = 0x0000_ffff_ffff_ffff;
/// `amdgpu_ring_max_ibs` (amdgpu_ring.c:57), called with the HW IP number as Linux does.
fn max_ibs(ip_type: u32) -> u32 {
    match ip_type {
        AMDGPU_HW_IP_GFX => 192,
        AMDGPU_HW_IP_COMPUTE => 125,
        AMDGPU_HW_IP_VCN_JPEG => 16,
        _ => 49,
    }
}

struct Chunk {
    id: u32,
    data: Vec<u8>,
}

impl Chunk {
    fn count<T: Wire>(&self) -> usize {
        self.data.len() / T::SIZE
    }
}

/// A gang member while the CS is parsed.
struct Member {
    entity: Arc<CtxEntity>,
    ibs: Vec<JobIb>,
    num_ibs: u32,
}

struct PostDep {
    obj: Arc<Syncobj>,
    point: u64,
}

/// `struct amdgpu_cs_parser`.
struct Parser<'a> {
    ctx: &'a Ctx<'a>,
    gpu: &'a dyn GpuDevice,
    gctx: Arc<GpuCtx>,
    chunks: Vec<Chunk>,
    members: Vec<Member>,
    leader: usize,
    uf: Option<(Arc<Bo>, u32)>,
    bo_list: Option<crate::bo_list::BoList>,
    deps: Vec<Arc<Fence>>,
    post: Option<Vec<PostDep>>,
    shadow: Option<GfxShadow>,
}

pub fn ioctl(ctx: &Ctx, gpu: &dyn GpuDevice, data: &mut [u8]) -> Result<i64, Errno> {
    let a = drm_amdgpu_cs::decode(data).ok_or(EINVAL)?.r#in();
    let r = run(ctx, gpu, &a);
    match r {
        Ok(seq) => {
            // `cs->out.handle = seq`: the other bytes of the union stay as they came.
            data[0..8].copy_from_slice(&seq.to_le_bytes());
            Ok(0)
        }
        Err(e) => {
            klog::event!(Debug, "drm", "cs.refused", minor = ctx.dev.render_minor(), ctx = a.ctx_id, chunks = a.num_chunks, errno = e);
            Err(e)
        }
    }
}

fn run(ctx: &Ctx, gpu: &dyn GpuDevice, a: &drm_amdgpu_cs_in) -> Result<u64, Errno> {
    if !gpu.accel_working() {
        return Err(EBUSY);
    }
    // amdgpu_cs_parser_init (:44)
    if a.num_chunks == 0 {
        return Err(EINVAL);
    }
    let gctx = ctx::lookup(ctx.file, a.ctx_id)?;
    if gctx.guilty() {
        return Err(ECANCELED);
    }
    let mut p = Parser { ctx, gpu, gctx, chunks: Vec::new(), members: Vec::new(), leader: 0, uf: None, bo_list: None, deps: Vec::new(), post: None, shadow: None };
    p.pass1(a)?;
    p.pass2()?;
    p.parser_bos(a)?;
    let vm = p.vm_handling()?;
    let r = p.submit(vm);
    // The backend runs what became ready only now, with no CS lock held (a fake job may take long).
    if r.is_ok() {
        gpu.kick();
    }
    r
}

/// Held from the implicit-sync look at the objects to the fences on them (Linux holds the objects'
/// reservations from `amdgpu_cs_parser_bos` to `amdgpu_cs_submit`): two CS writing one object see each
/// other in the order they reach the backend. One for all devices (an object may be shared by dma-buf); it
/// is short, since `GpuDevice::submit` only queues.
static CS_SYNC: ksync::Mutex<()> = ksync::Mutex::named("drm.cs.sync", ());

impl Parser<'_> {
    /// `amdgpu_cs_job_idx` (:73): the gang member of an IB's entity, a new one while the gang has room.
    fn job_idx(&mut self, ib: &drm_amdgpu_cs_chunk_ib) -> Result<usize, Errno> {
        let e = self.gctx.entity(self.gpu, ib.ip_type, ib.ip_instance, ib.ring)?;
        if let Some(i) = self.members.iter().position(|m| Arc::ptr_eq(&m.entity, &e)) {
            return Ok(i);
        }
        if self.members.len() == GANG_SIZE {
            return Err(EINVAL);
        }
        self.members.try_reserve(1).map_err(|_| ENOMEM)?;
        self.members.push(Member { entity: e, ibs: Vec::new(), num_ibs: 0 });
        Ok(self.members.len() - 1)
    }

    /// `amdgpu_cs_pass1` (:175): the chunk array and every chunk's data from the program, the first look.
    fn pass1(&mut self, a: &drm_amdgpu_cs_in) -> Result<(), Errno> {
        let n = a.num_chunks as u64;
        let ptrs = copy_in(self.ctx, a.chunks, n * 8)?;
        self.chunks.try_reserve_exact(n as usize).map_err(|_| ENOMEM)?;
        for i in 0..n as usize {
            let at = u64::from_le_bytes(ptrs[i * 8..i * 8 + 8].try_into().unwrap_or([0; 8]));
            let h = copy_in(self.ctx, at, drm_amdgpu_cs_chunk::SIZE as u64)?;
            let h = rec::<drm_amdgpu_cs_chunk>(&h, 0);
            let data = copy_in(self.ctx, h.chunk_data, h.length_dw as u64 * 4)?;
            let size = data.len();
            match h.chunk_id {
                AMDGPU_CHUNK_ID_IB => {
                    if size < drm_amdgpu_cs_chunk_ib::SIZE {
                        return Err(EINVAL);
                    }
                    let ib = rec::<drm_amdgpu_cs_chunk_ib>(&data, 0);
                    let r = self.job_idx(&ib)?;
                    let m = &mut self.members[r];
                    if m.num_ibs >= max_ibs(ib.ip_type) {
                        return Err(EINVAL);
                    }
                    m.num_ibs += 1;
                    self.leader = r;
                }
                AMDGPU_CHUNK_ID_FENCE => {
                    // CachyOS: one FENCE chunk per CS (6.6 took the last).
                    if size < drm_amdgpu_cs_chunk_fence::SIZE || self.uf.is_some() {
                        return Err(EINVAL);
                    }
                    // amdgpu_cs_p1_user_fence (:125): one page, the 8 bytes inside it (no alignment rule).
                    let f = rec::<drm_amdgpu_cs_chunk_fence>(&data, 0);
                    let bo = self.ctx.file.bo(f.handle).ok_or(EINVAL)?;
                    if bo.size() != 4096 || f.offset as u64 > bo.size() - 8 || bo.is_userptr() {
                        return Err(EINVAL);
                    }
                    self.uf = Some((bo, f.offset));
                }
                AMDGPU_CHUNK_ID_BO_HANDLES => {
                    // CachyOS: one BO_HANDLES chunk per CS (6.6 took the last).
                    if size < drm_amdgpu_bo_list_in::SIZE || self.bo_list.is_some() {
                        return Err(EINVAL);
                    }
                    let li = rec::<drm_amdgpu_bo_list_in>(&data, 0);
                    let es = bo_list_entries(self.ctx, &li)?;
                    self.bo_list = Some(Arc::new(bo_list_create(self.ctx, &es)?));
                }
                // CachyOS checks the shadow chunk's size here (6.6 read past a short one).
                AMDGPU_CHUNK_ID_CP_GFX_SHADOW if size < drm_amdgpu_cs_chunk_cp_gfx_shadow::SIZE => return Err(EINVAL),
                AMDGPU_CHUNK_ID_DEPENDENCIES
                | AMDGPU_CHUNK_ID_SYNCOBJ_IN
                | AMDGPU_CHUNK_ID_SYNCOBJ_OUT
                | AMDGPU_CHUNK_ID_SCHEDULED_DEPENDENCIES
                | AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_WAIT
                | AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_SIGNAL
                | AMDGPU_CHUNK_ID_CP_GFX_SHADOW => {}
                _ => return Err(EINVAL),
            }
            self.chunks.push(Chunk { id: h.chunk_id, data });
        }
        if self.members.is_empty() {
            return Err(EINVAL);
        }
        for m in &mut self.members {
            m.ibs.try_reserve_exact(m.num_ibs as usize).map_err(|_| ENOMEM)?;
        }
        // The VRAM of the context's generation is gone (amdgpu_vm_generation).
        if self.gctx.generation() != self.gpu.vram_lost_counter() {
            return Err(ECANCELED);
        }
        Ok(())
    }

    /// `amdgpu_cs_pass2` (:588): the chunks again, in their order.
    fn pass2(&mut self) -> Result<(), Errno> {
        let mut de = 0u32;
        let chunks = core::mem::take(&mut self.chunks);
        let r = (|| {
            for c in &chunks {
                match c.id {
                    AMDGPU_CHUNK_ID_IB => self.p2_ib(c, &mut de)?,
                    AMDGPU_CHUNK_ID_DEPENDENCIES | AMDGPU_CHUNK_ID_SCHEDULED_DEPENDENCIES => self.p2_dependencies(c)?,
                    AMDGPU_CHUNK_ID_SYNCOBJ_IN => {
                        for i in 0..c.count::<drm_amdgpu_cs_chunk_sem>() {
                            let d = rec::<drm_amdgpu_cs_chunk_sem>(&c.data, i);
                            self.syncobj_dep(d.handle, 0, 0)?;
                        }
                    }
                    AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_WAIT => {
                        for i in 0..c.count::<drm_amdgpu_cs_chunk_syncobj>() {
                            let d = rec::<drm_amdgpu_cs_chunk_syncobj>(&c.data, i);
                            self.syncobj_dep(d.handle, d.point, d.flags)?;
                        }
                    }
                    AMDGPU_CHUNK_ID_SYNCOBJ_OUT => {
                        let n = c.count::<drm_amdgpu_cs_chunk_sem>();
                        self.post_deps(n, |i| (rec::<drm_amdgpu_cs_chunk_sem>(&c.data, i).handle, 0))?;
                    }
                    AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_SIGNAL => {
                        let n = c.count::<drm_amdgpu_cs_chunk_syncobj>();
                        self.post_deps(n, |i| {
                            let d = rec::<drm_amdgpu_cs_chunk_syncobj>(&c.data, i);
                            (d.handle, d.point)
                        })?;
                    }
                    AMDGPU_CHUNK_ID_CP_GFX_SHADOW => {
                        let s = rec::<drm_amdgpu_cs_chunk_cp_gfx_shadow>(&c.data, 0);
                        if s.flags & !(AMDGPU_CS_CHUNK_CP_GFX_SHADOW_FLAGS_INIT_SHADOW as u64) != 0 {
                            return Err(EINVAL);
                        }
                        let init = s.flags & AMDGPU_CS_CHUNK_CP_GFX_SHADOW_FLAGS_INIT_SHADOW as u64 != 0;
                        self.shadow = Some(GfxShadow { shadow_va: s.shadow_va, csa_va: s.csa_va, gds_va: s.gds_va, init });
                    }
                    _ => {}
                }
            }
            Ok(())
        })();
        self.chunks = chunks;
        r
    }

    /// `amdgpu_cs_p2_ib` (:326): one preemptible DE IB per GFX submission. CachyOS refuses the CE flag on any IB
    /// (GFX 11 and later have no constant engine; seen on the RX 9070 XT, applied to the 780M too), so 6.6's
    /// count of preemptible CE IBs cannot be reached.
    fn p2_ib(&mut self, c: &Chunk, de: &mut u32) -> Result<(), Errno> {
        let ib = rec::<drm_amdgpu_cs_chunk_ib>(&c.data, 0);
        let r = self.job_idx(&ib)?;
        if ib.flags & AMDGPU_IB_FLAG_CE != 0 {
            return Err(EINVAL);
        }
        if ib.ip_type == AMDGPU_HW_IP_GFX && ib.flags & AMDGPU_IB_FLAG_PREEMPT != 0 {
            *de += 1;
            if *de > 1 {
                return Err(EINVAL);
            }
        }
        self.members[r].ibs.push(JobIb { va: ib.va_start & HOLE_MASK, len_dw: ib.ib_bytes / 4, flags: ib.flags });
        Ok(())
    }

    /// `amdgpu_cs_p2_dependencies` (:381): a job of a context of this file by its sequence number; a scheduled
    /// dependency waits only until that job started.
    fn p2_dependencies(&mut self, c: &Chunk) -> Result<(), Errno> {
        for i in 0..c.count::<drm_amdgpu_cs_chunk_dep>() {
            let d = rec::<drm_amdgpu_cs_chunk_dep>(&c.data, i);
            let dc = ctx::lookup(self.ctx.file, d.ctx_id)?;
            let e = dc.entity(self.gpu, d.ip_type, d.ip_instance, d.ring)?;
            let Some(s) = e.fence(d.handle)? else { continue };
            let f = if c.id == AMDGPU_CHUNK_ID_SCHEDULED_DEPENDENCIES { s.scheduled } else { s.finished };
            self.add_dep(f)?;
        }
        Ok(())
    }

    /// `amdgpu_syncobj_lookup_and_add` (:432): `drm_syncobj_find_fence` (ENOENT, EINVAL, WAIT_FOR_SUBMIT).
    fn syncobj_dep(&mut self, handle: u32, point: u64, flags: u32) -> Result<(), Errno> {
        let f = syncobj::find_fence(self.ctx, handle, point, flags)?;
        self.add_dep(f)
    }

    fn add_dep(&mut self, f: Arc<Fence>) -> Result<(), Errno> {
        if crate::fence::is_signaled(&f) {
            return Ok(());
        }
        self.deps.try_reserve(1).map_err(|_| ENOMEM)?;
        self.deps.push(f);
        Ok(())
    }

    /// `amdgpu_cs_p2_syncobj_out`/`_timeline_signal` (:491/:525): one such chunk per CS (EINVAL for a second, even
    /// after an empty one); every handle must exist (EINVAL).
    fn post_deps(&mut self, n: usize, at: impl Fn(usize) -> (u32, u64)) -> Result<(), Errno> {
        if self.post.is_some() {
            return Err(EINVAL);
        }
        let mut v = Vec::new();
        v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
        for i in 0..n {
            let (h, point) = at(i);
            let obj = self.ctx.file.syncobj(h).ok_or(EINVAL)?;
            v.push(PostDep { obj, point });
        }
        self.post = Some(v);
        Ok(())
    }

    /// `amdgpu_cs_parser_bos` (:833): a BO list by handle from the file's table (ENOENT), not together with a
    /// BO_HANDLES chunk (EINVAL); then the userptr objects of the list: the caller's own address space (EPERM,
    /// :922) and their pages again if the program unmapped or changed them since (`amdgpu_ttm_tt_get_user_pages`,
    /// :868: EFAULT when they are gone).
    fn parser_bos(&mut self, a: &drm_amdgpu_cs_in) -> Result<(), Errno> {
        if a.bo_list_handle != 0 {
            if self.bo_list.is_some() {
                return Err(EINVAL);
            }
            self.bo_list = Some(self.ctx.file.bo_lists.get(a.bo_list_handle)?);
        }
        for (bo, _) in self.bo_list.as_deref().map(|v| v.as_slice()).unwrap_or(&[]) {
            if bo.is_userptr() {
                crate::userptr::check_mm(self.ctx, bo)?;
                crate::userptr::revalidate(self.ctx, bo)?;
            }
        }
        Ok(())
    }

    /// `amdgpu_cs_vm_handling` (:1085): the VM's queued page-table jobs run first (their error fails the CS);
    /// the jobs wait for the last page-table update.
    fn vm_handling(&mut self) -> Result<Arc<dyn crate::gpu::GpuVm>, Errno> {
        let vm = &self.ctx.file.vm;
        vm.backend(self.gpu)?;
        vm.flush()?;
        if let Some(f) = vm.last_update() {
            self.add_dep(f)?;
        }
        vm.backend_vm().ok_or(ENODEV)
    }

    /// `amdgpu_cs_sync_rings` (:1171): implicit sync with every object of the list and the user fence object:
    /// the fences of other files (AMDGPU_SYNC_NE_OWNER, :1194), an EXPLICIT_SYNC object only the kernel's own.
    /// The caller holds `CS_SYNC`.
    fn sync_rings(&mut self, bos: &[Arc<Bo>]) -> Result<(), Errno> {
        let owner = self.ctx.file.client_id();
        for b in bos {
            for f in b.cs_deps(true, owner) {
                self.add_dep(f)?;
            }
        }
        Ok(())
    }

    fn objects(&self) -> Result<Vec<Arc<Bo>>, Errno> {
        let list = self.bo_list.as_deref().map(|v| v.as_slice()).unwrap_or(&[]);
        let mut v = Vec::new();
        v.try_reserve_exact(list.len() + 1).map_err(|_| ENOMEM)?;
        v.extend(list.iter().map(|(b, _)| b.clone()));
        if let Some((b, _)) = &self.uf {
            v.push(b.clone());
        }
        Ok(v)
    }

    /// `amdgpu_cs_submit` (:1248): sequence numbers and fences under the entities' submit locks (taken in id
    /// order), then `CS_SYNC` for the implicit sync, the backend's queue, then what a program sees: the fences
    /// on the objects, the out-syncobjs, the number.
    fn submit(mut self, vm: Arc<dyn crate::gpu::GpuVm>) -> Result<u64, Errno> {
        let bos = self.objects()?;
        let mut ents: Vec<Arc<CtxEntity>> = self.members.iter().map(|m| m.entity.clone()).collect();
        ents.sort_by_key(|e| e.job.id());
        let mut guards = Vec::new();
        guards.try_reserve_exact(ents.len()).map_err(|_| ENOMEM)?;
        for e in &ents {
            guards.push(e.submit.lock());
        }
        let sync = CS_SYNC.lock();
        self.sync_rings(&bos)?;
        // The leader goes last in the gang (the backend's contract).
        let leader = self.leader;
        let mut idx: Vec<usize> = (0..self.members.len()).filter(|&i| i != leader).collect();
        idx.push(leader);
        let mut jobs = Vec::new();
        jobs.try_reserve_exact(idx.len()).map_err(|_| ENOMEM)?;
        let mut pushed: Vec<(usize, u64)> = Vec::new();
        pushed.try_reserve_exact(idx.len()).map_err(|_| ENOMEM)?;
        let mut seq = 0;
        for &i in &idx {
            let m = &mut self.members[i];
            let r = m.entity.push(i == leader);
            let (s, slot) = match r {
                Ok(x) => x,
                Err(e) => {
                    for &(k, s) in pushed.iter().rev() {
                        self.members[k].entity.unpush(s);
                    }
                    return Err(e);
                }
            };
            pushed.push((i, s));
            if i == leader {
                seq = s;
            }
            jobs.push(Job { entity: m.entity.job.clone(), ibs: core::mem::take(&mut m.ibs), shadow: self.shadow, scheduled: slot.scheduled, finished: slot.finished });
        }
        let finished: Vec<Arc<Fence>> = jobs.iter().map(|j| j.finished.clone()).collect();
        let user_fence = self.uf.as_ref().map(|(b, off)| UserFence { mem: b.memory().clone(), offset: *off as u64, value: seq });
        let sub = Submission { jobs, vm, deps: core::mem::take(&mut self.deps), user_fence, bos: bos.clone(), generation: self.gctx.generation() };
        let ibs: usize = sub.jobs.iter().map(|j| j.ibs.len()).sum();
        if let Err(e) = self.gpu.submit(sub) {
            for &(k, s) in pushed.iter().rev() {
                self.members[k].entity.unpush(s);
            }
            klog::event!(Warn, "drm", "cs.backend_refused", errno = e);
            return Err(e);
        }
        let Some(lf) = finished.last() else { return Err(EINVAL) };
        let owner = self.ctx.file.client_id();
        // Everybody but the leader as a reader, the leader as the writer; the VM root too.
        let resvs = bos.iter().map(|b| b.resv().clone()).chain(core::iter::once(self.ctx.file.gem.vm_resv.clone()));
        for r in resvs {
            for f in &finished[..finished.len() - 1] {
                note(r.add_fence_owned(f, Usage::Read, owner));
            }
            note(r.add_fence_owned(lf, Usage::Write, owner));
        }
        for d in self.post.take().unwrap_or_default() {
            if d.point != 0 {
                d.obj.add_point(lf.clone(), d.point);
            } else {
                d.obj.replace_fence(Some(lf.clone()));
            }
        }
        drop(sync);
        drop(guards);
        klog::event!(Debug, "drm", "cs.submit", minor = self.ctx.dev.render_minor(), seq = seq, jobs = finished.len(), ibs = ibs, bos = bos.len());
        Ok(seq)
    }
}

/// Linux reserves the fence slots before it submits; here a slot that finds no memory leaves the object without
/// the job's fence (a later reader does not wait for it), said once as a Warn.
fn note(r: Result<(), Errno>) {
    if let Err(e) = r {
        klog::event!(Warn, "drm", "cs.resv_full", errno = e; "a job's fence did not fit on an object: implicit sync misses it");
    }
}
