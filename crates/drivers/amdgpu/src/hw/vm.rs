//! GX-8a: a client VM on the chip (`drm::gpu::GpuVm`, Linux `amdgpu_vm`): real GFX 11 page tables in the carve-out,
//! built by GR-1's core (`amdgpu_fmt::vm`) and written by the CPU, uncached, one store per entry (the path the chip
//! ran under `until=vm`; Linux's `amdgpu_vm_cpu.c`, not SDMA). PTE flags as gmc_v11_0.c:521 `gmc_v11_0_get_vm_pte`
//! and amdgpu_ttm.c:1084 set them: MTYPE from the VA flags (default NC; the UC of COHERENT/UNCACHED objects,
//! gmc_v11_0.c:544, does not arise: GEM_CREATE refuses those flags, only KFD sets them), SYSTEM and SNOOPED by where
//! the memory is, read/write/execute from the VA flags, PRT without VALID.
//!
//! A job takes a VMID with `begin_job` (Linux amdgpu_ids.c:397 `amdgpu_vmid_grab`): the slot this VM still owns with
//! the same page directory, flushed if its tables changed since; else the least recently used idle one, flushed;
//! none free: the caller waits for a job to end. Dropping the `VmJob` means the job has finished on the GPU, in
//! thread context; `VmJob::cancel` means it never reached a ring.
//!
//! What the tables no longer reach (tables and memory of a Clear) stays until no job that began before the change
//! runs any more (Linux: the page-table BOs share the root's reservation, so TTM frees them after the VM's fences).
//! A job that begins after the change sees the new tables through its own flush, and a VMID that goes to another VM
//! is flushed too, so no job can use a stale translation of memory freed here.

use super::heap::{Heaps, TablePage};
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use amdgpu_fmt::addr::sign_extend;
use amdgpu_fmt::ids::{Owner, PasidAlloc, VmidMgr};
use amdgpu_fmt::pte::{MapFlags, Target};
use amdgpu_fmt::store::{TABLE_BYTES, TableAddr, TableRead, TableStore};
use amdgpu_fmt::vm::{Backing, Vm};
use amdgpu_fmt::walk::{self, Walk, WalkError};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use drm::dma_buf::Exporter;
use drm::fence::{self, Fence, FenceCb, Registration};
use drm::gpu::{GpuShared, GpuVm, PtOp, VmFault, VmUpdate};
use ksync::{Mutex, SpinLock};
use linux::errno::{EFAULT, EINVAL, EIO, ENOMEM, ESRCH};
use vfs::Errno;

const PAGE: u64 = 4096;
const PAGE_SHIFT: u32 = 12;
/// VMIDs the kernel's jobs take (gmc_v11_0.c:854 first_kfd_vmid = 8; 8-15 are MES's process queues, VMID 0 the GART).
pub const KERNEL_VMIDS: core::ops::Range<u32> = 1..8;
/// Linux's PASID width for a client VM (amdgpu_kms.c:1274 `amdgpu_pasid_alloc(16)`).
const PASID_BITS: u32 = 16;

/// The device's VMIDs and PASIDs.
pub struct VmIds {
    mgr: SpinLock<IdState>,
    pasids: SpinLock<PasidAlloc>,
    next_owner: AtomicU64,
}

struct IdState {
    mgr: VmidMgr,
    /// The manager holds a reserved slot (`GpuShared::reserved_vmid_owner` is set).
    reserved: bool,
}

impl VmIds {
    pub fn new() -> Option<VmIds> {
        let mgr = VmidMgr::new(KERNEL_VMIDS.start, KERNEL_VMIDS.end - KERNEL_VMIDS.start).ok()?;
        Some(VmIds { mgr: SpinLock::named("amdgpu.hw.vmids", IdState { mgr, reserved: false }), pasids: SpinLock::named("amdgpu.hw.pasids", PasidAlloc::new()), next_owner: AtomicU64::new(1) })
    }

    /// PASIDs in use.
    /// GX-9a: after a full reset every VMID needs a flush (amdgpu_ids.c:532).
    pub fn reset_all(&self) {
        self.mgr.lock().mgr.reset_all();
    }

    pub fn pasids(&self) -> usize {
        self.pasids.lock().in_use()
    }
}

/// Page tables of one VM in the table heap. A store that fails sets `failed`: the VM goes broken (EIO), since the
/// tables no longer say what its bookkeeping says.
struct Tables {
    heaps: Arc<Heaps>,
    /// Host-physical address of a table -> its page.
    pages: BTreeMap<u64, TablePage>,
    /// Tables this VM may hold (half the carve-out in tables stays for the others).
    budget: usize,
    failed: bool,
}

impl TableRead for Tables {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        let p = self.pages.get(&table)?;
        ((index as u64) < TABLE_BYTES / 8).then(|| p.win.read64(p.at + index as u64 * 8)).flatten()
    }
}

impl TableStore for Tables {
    fn alloc(&mut self) -> Option<TableAddr> {
        if self.pages.len() >= self.budget {
            return None;
        }
        let p = self.heaps.alloc_table()?;
        let Some(phys) = self.heaps.carve.phys_of(p.offset) else {
            self.heaps.free_table(p);
            return None;
        };
        self.pages.insert(phys, p);
        Some(TableAddr { phys, target: Target::Local })
    }
    fn available(&self) -> usize {
        self.budget.saturating_sub(self.pages.len()).min(self.heaps.table_room())
    }
    fn free(&mut self, table: TableAddr) {
        if let Some(p) = self.pages.remove(&table.phys) {
            self.heaps.free_table(p);
        }
    }
    fn write(&mut self, table: u64, index: usize, value: u64) {
        let ok = self.pages.get(&table).is_some_and(|p| (index as u64) < TABLE_BYTES / 8 && p.win.write64(p.at + index as u64 * 8, value));
        self.failed |= !ok;
    }
    fn fill(&mut self, table: u64, value: u64) {
        let ok = self.pages.get(&table).is_some_and(|p| p.win.fill(p.at, TABLE_BYTES, value));
        self.failed |= !ok;
    }
}

/// What a range reaches: memory kept alive (None for PRT): the object that owns the frames in the entries.
#[derive(Clone)]
struct Piece {
    pages: u64,
    mem: Option<Arc<dyn Exporter>>,
    first: u64,
}

/// One `update_after` that could not run at once.
struct Deferred {
    ops: Vec<PtOp>,
    /// None: waits only for the updates before it.
    dep: Option<Arc<Fence>>,
    done: Arc<Fence>,
    _reg: Option<Registration>,
}

struct State {
    store: Tables,
    tables: Option<Vm>,
    /// Start page -> what the GPU can reach (keeps the memory).
    held: BTreeMap<u64, Piece>,
    /// Pieces a Clear cut off, with the sequence number of that change.
    garbage: VecDeque<(u64, Piece)>,
    deferred: VecDeque<Deferred>,
    broken: bool,
}

/// The sequence numbers jobs began at, with their counts.
#[derive(Default)]
struct Jobs(BTreeMap<u64, u32>);

/// Runs the queue when a dependency signals (the VM may be gone by then).
struct Wake(Weak<HwVm>);

impl FenceCb for Wake {
    fn call(&self) {
        if let Some(vm) = self.0.upgrade() {
            vm.pump();
        }
    }
}

pub struct HwVm {
    me: Weak<HwVm>,
    ids: Arc<VmIds>,
    shared: Arc<GpuShared>,
    owner: Owner,
    client: u64,
    pasid: u32,
    heaps: Arc<Heaps>,
    st: Mutex<State>,
    jobs: SpinLock<Jobs>,
    /// A thread is running the deferred queue.
    pumping: AtomicBool,
    fence_ctx: u64,
    fence_seq: AtomicU64,
    fault: SpinLock<Option<VmFault>>,
    live: Arc<AtomicU64>,
}

/// A job's hold on the VM: its VMID, and that what it can reach stays. Dropping it says the job finished on the GPU
/// (in thread context: it waits for the VM's lock to free what the job kept); a job that never reached a ring ends with
/// `cancel`.
pub struct VmJob {
    vm: Arc<HwVm>,
    pub vmid: u32,
    /// The job must load `pd` and invalidate the VMID's TLB before it runs.
    pub needs_flush: bool,
    /// The page-table base register value.
    pub pd: u64,
    pub pasid: u32,
    seq: u64,
}

impl VmJob {
    /// The job never reached a ring (ring full, a refused submit). `grab` wrote its flush down as done when it handed
    /// it out; one that never ran must not count, or the next job of this VM runs on stale TLB entries, translations
    /// of freed memory among them. The VM's slots forget it, so its next job flushes (`VmidMgr::forget`; a whole VM
    /// and not this slot alone, as one flush too many costs nothing).
    pub fn cancel(self) {
        if self.needs_flush {
            klog::event!(Debug, "gx", "hw.job_flush_lost", vmid = self.vmid, pasid = self.pasid);
            self.vm.ids.mgr.lock().mgr.forget(self.vm.owner);
        }
    }
}

impl Drop for VmJob {
    fn drop(&mut self) {
        if let Err(e) = self.vm.ids.mgr.lock().mgr.job_done(self.vmid) {
            klog::event!(Error, "gx", "hw.vmid_done_failed", vmid = self.vmid, why = alloc::format!("{e:?}").as_str());
        }
        {
            let mut j = self.vm.jobs.lock();
            if let Some(n) = j.0.get_mut(&self.seq) {
                *n -= 1;
                if *n == 0 {
                    j.0.remove(&self.seq);
                }
            }
        }
        // Blocking: a reader or an update can hold the lock, and no later update has to come. What this job kept up
        // (a closed object's memory, its charge) must go now.
        let gone = self.vm.reclaim(&mut self.vm.st.lock());
        drop(gone);
    }
}

impl HwVm {
    pub(super) fn new(ids: Arc<VmIds>, shared: Arc<GpuShared>, heaps: Arc<Heaps>, client: u64, live: Arc<AtomicU64>) -> Result<Arc<HwVm>, Errno> {
        let budget = ((heaps.carve.size() / 2 / TABLE_BYTES) as usize).max(1);
        let mut store = Tables { heaps: heaps.clone(), pages: BTreeMap::new(), budget, failed: false };
        let vm = Vm::new(&mut store, heaps.carve).map_err(|e| e.errno())?;
        let pasid = match ids.pasids.lock().alloc(PASID_BITS) {
            Ok(p) => p,
            Err(e) => {
                vm.destroy(&mut store);
                return Err(e.errno().into());
            }
        };
        heaps.io.barrier();
        let owner = Owner(ids.next_owner.fetch_add(1, Ordering::Relaxed));
        let st = State { store, tables: Some(vm), held: BTreeMap::new(), garbage: VecDeque::new(), deferred: VecDeque::new(), broken: false };
        live.fetch_add(1, Ordering::AcqRel);
        klog::event!(Debug, "gx", "hw.vm_created", pasid = pasid, client = client);
        Ok(Arc::new_cyclic(|me| HwVm {
            me: me.clone(),
            ids,
            shared,
            owner,
            client,
            pasid,
            heaps,
            st: Mutex::named("amdgpu.hw.vm", st),
            jobs: SpinLock::named("amdgpu.hw.vm_jobs", Jobs::default()),
            pumping: AtomicBool::new(false),
            fence_ctx: fence::context_alloc(1),
            fence_seq: AtomicU64::new(0),
            fault: SpinLock::named("amdgpu.hw.vm_fault", None),
            live,
        }))
    }

    pub fn pasid(&self) -> u32 {
        self.pasid
    }

    /// The VM as `begin_job` wants it, from what `GpuVm::as_any` gives.
    pub fn arc(&self) -> Option<Arc<HwVm>> {
        self.me.upgrade()
    }

    /// A VMID for one job (GX-8b calls it when the job is about to run). `Ok(None)`: every VMID is busy, or the
    /// reserved one is another VM's with a job on it: try again when a job ended (Linux waits on its fence).
    pub fn begin_job(self: &Arc<Self>) -> Result<Option<VmJob>, Errno> {
        let g = self.st.lock();
        let vm = g.tables.as_ref().filter(|_| !g.broken).ok_or(EIO)?;
        let (pd, seq) = (vm.pd_addr(), vm.tlb_seq());
        let holder = self.shared.reserved_vmid_owner();
        let grab = {
            let mut ids = self.ids.mgr.lock();
            // The uAPI keeps the holder; the slot follows it here (amdgpu_vmid_alloc_reserved, _free_reserved).
            if holder.is_some() != ids.reserved {
                let r = if holder.is_some() { ids.mgr.reserve().map(|_| ()) } else { ids.mgr.unreserve() };
                r.map_err(|e| e.errno())?;
                ids.reserved = holder.is_some();
            }
            ids.mgr.grab(self.owner, pd, seq, holder == Some(self.client)).map_err(|e| e.errno())?
        };
        let Some(grab) = grab else { return Ok(None) };
        *self.jobs.lock().0.entry(seq).or_insert(0) += 1;
        Ok(Some(VmJob { vm: self.clone(), vmid: grab.vmid, needs_flush: grab.needs_flush, pd, pasid: self.pasid, seq }))
    }

    /// Frees what no running job can reach; the memory drops in the caller, outside the state lock.
    fn reclaim(&self, g: &mut State) -> Vec<Piece> {
        let State { store, tables, garbage, .. } = g;
        let Some(vm) = tables.as_mut() else { return Vec::new() };
        let safe = self.jobs.lock().0.keys().next().copied().unwrap_or_else(|| vm.tlb_seq());
        vm.reclaim(store, safe);
        let mut gone = Vec::new();
        while garbage.front().is_some_and(|(s, _)| *s <= safe) {
            if let Some((_, p)) = garbage.pop_front() {
                gone.push(p);
            }
        }
        gone
    }

    fn apply(&self, g: &mut State, op: &PtOp) -> Result<(), Errno> {
        if g.broken {
            return Err(EIO);
        }
        let r = apply_op(&self.heaps, g, op);
        if core::mem::take(&mut g.store.failed) {
            g.broken = true;
            let (va, pages) = op.range();
            klog::event!(Error, "gx", "hw.vm_store_failed", va = klog::Hex(va), pages = pages; "a page-table store did not reach the carve-out: the VM answers EIO from now on");
            return Err(EIO);
        }
        r
    }

    /// `ops` in order, up to the first that fails.
    fn apply_all(&self, g: &mut State, ops: &[PtOp]) -> VmUpdate {
        let mut done = 0;
        let mut error = None;
        for op in ops {
            if let Err(e) = self.apply(g, op) {
                let (va, pages) = op.range();
                klog::event!(Warn, "gx", "hw.vm_update_failed", va = klog::Hex(va), pages = pages, errno = e);
                error = Some(e);
                break;
            }
            done += 1;
        }
        // The tables are in memory before any flush a later job makes.
        self.heaps.io.barrier();
        VmUpdate { done, error, fence: None }
    }

    /// Runs the queue's head while its dependency signaled, each update in one piece under the lock.
    fn drain(&self) {
        loop {
            let (d, error, gone) = {
                let mut g = self.st.lock();
                match g.deferred.front() {
                    Some(h) if h.dep.as_ref().is_none_or(fence::is_signaled) => {}
                    _ => return,
                }
                let Some(d) = g.deferred.pop_front() else { return };
                let r = self.apply_all(&mut g, &d.ops);
                let gone = self.reclaim(&mut g);
                (d, r.error, gone)
            };
            drop(gone);
            match error {
                Some(e) => {
                    klog::event!(Error, "gx", "hw.vm_deferred_failed", ops = d.ops.len(), errno = e; "a deferred page-table update failed: its fence ends with the error");
                    d.done.signal_error(e as i32);
                }
                None => {
                    d.done.signal();
                }
            }
        }
    }

    /// A dependency signaled: the writes may be many, and a fence callback runs in the signaler's thread, so a thread
    /// of its own does them. Host tests have no threads; a failed spawn runs them here (late is worse than slow).
    fn pump(&self) {
        if self.pumping.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(me) = self.me.upgrade() else {
            self.pumping.store(false, Ordering::Release);
            return;
        };
        #[cfg(test)]
        me.pump_run();
        #[cfg(not(test))]
        if let Err(e) = sched::spawn("amdgpu-vm", sched::Class::Normal, move || me.pump_run()) {
            klog::event!(Warn, "gx", "hw.vm_pump_spawn_failed", why = alloc::format!("{e:?}").as_str());
            self.pump_run();
        }
    }

    fn pump_run(&self) {
        loop {
            self.drain();
            self.pumping.store(false, Ordering::Release);
            // A dependency that signaled between the last look and the store found `pumping` set.
            let ready = self.st.lock().deferred.front().is_some_and(|h| h.dep.as_ref().is_none_or(fence::is_signaled));
            if !ready || self.pumping.swap(true, Ordering::AcqRel) {
                return;
            }
        }
    }

    /// DR-9: a job of this VM faulted (`amdgpu_vm_update_fault_cache`; GX-8b's fault path calls it).
    pub fn note_fault(&self, f: VmFault) {
        *self.fault.lock() = Some(f);
    }

    /// The tables' walk at `va` (GR-1's software walker, reading the entries back through the CPU window).
    pub fn walk(&self, va: u64) -> Result<Walk, WalkError> {
        let g = self.st.lock();
        let root = g.tables.as_ref().map_or(0, |v| v.root().phys);
        walk::walk(&g.store, root, sign_extend(va & 0x0000_ffff_ffff_ffff))
    }

    /// The page-table base register value (the root's PDE).
    pub fn pd_addr(&self) -> u64 {
        self.st.lock().tables.as_ref().map_or(0, Vm::pd_addr)
    }

    pub fn tlb_seq(&self) -> u64 {
        self.st.lock().tables.as_ref().map_or(0, Vm::tlb_seq)
    }

    /// Tables reachable from the root, and tables and pieces waiting for jobs to end.
    pub fn counts(&self) -> VmCounts {
        let g = self.st.lock();
        let (tables, pending) = g.tables.as_ref().map_or((0, 0), |v| (v.table_count(), v.pending_count()));
        VmCounts { tables, pending, garbage: g.garbage.len(), held: g.held.len(), deferred: g.deferred.len(), pages: g.store.pages.len() }
    }

    /// The memory `va` reaches: the object and the page in it, as the VM's own bookkeeping says.
    pub fn held_at(&self, va: u64) -> Option<(Option<Arc<dyn Exporter>>, u64)> {
        let p = (va & 0x0000_ffff_ffff_ffff) >> PAGE_SHIFT;
        let g = self.st.lock();
        let (&s, piece) = g.held.range(..=p).next_back()?;
        (p < s + piece.pages).then(|| (piece.mem.clone(), piece.first + (p - s)))
    }
}

/// See `HwVm::counts`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmCounts {
    pub tables: usize,
    pub pending: usize,
    pub garbage: usize,
    pub held: usize,
    pub deferred: usize,
    pub pages: usize,
}

/// How many held pieces `[s, e)` (pages) touches.
fn touched(held: &BTreeMap<u64, Piece>, s: u64, e: u64) -> usize {
    let first = held.range(..s).next_back().is_some_and(|(k, p)| *k + p.pages > s);
    usize::from(first) + held.range(s..e).count()
}

/// Removes `[s, e)` (pages) from the held map, keeping what sticks out; the cut pieces go to the garbage at `seq`.
fn cut(held: &mut BTreeMap<u64, Piece>, s: u64, e: u64, garbage: &mut VecDeque<(u64, Piece)>, seq: u64) {
    loop {
        let first = held.range(..s).next_back().filter(|(k, p)| **k + p.pages > s).map(|(k, _)| *k);
        let Some(k) = first.or_else(|| held.range(s..e).next().map(|(k, _)| *k)) else { break };
        let Some(p) = held.remove(&k) else { break };
        let end = k + p.pages;
        if k < s {
            held.insert(k, Piece { pages: s - k, ..p.clone() });
        }
        if end > e {
            held.insert(e, Piece { pages: end - e, mem: p.mem.clone(), first: p.first + (e - k) });
        }
        garbage.push_back((seq, p));
    }
}

/// Writes the entries of a Map; `Ok(what the entries reach)`. One snapshot of the frames for all pages (a userptr
/// object can change them at any time): the frames in the entries are the frames kept.
fn map_memory(heaps: &Heaps, vm: &mut Vm, store: &mut Tables, op: &PtOp) -> Result<Arc<dyn Exporter>, Errno> {
    let PtOp::Map { va, pages, mem, first, uswc, flags, .. } = op else { return Err(EINVAL) };
    let mem = mem.frames().unwrap_or_else(|| mem.clone());
    let len = pages.checked_mul(PAGE).ok_or(EINVAL)?;
    let (f, _) = MapFlags::from_uapi(*flags);
    let start = mem.page_phys(*first).ok_or(EFAULT)?;
    if heaps.carve.contains_phys(start, PAGE) {
        // VRAM: contiguous by construction (`VramBo`); a range that says otherwise never gets a local PTE.
        let last = first.checked_add(*pages - 1).and_then(|l| mem.page_phys(l)).ok_or(EFAULT)?;
        if last != start + (*pages - 1) * PAGE {
            return Err(EINVAL);
        }
        vm.map(store, sign_extend(*va), len, Backing::Local { phys: start }, f).map_err(|e| e.errno())?;
    } else {
        let n = usize::try_from(*pages).map_err(|_| ENOMEM)?;
        let mut phys = Vec::new();
        phys.try_reserve_exact(n).map_err(|_| ENOMEM)?;
        for i in 0..*pages {
            phys.push(first.checked_add(i).and_then(|k| mem.page_phys(k)).ok_or(EFAULT)?);
        }
        vm.map(store, sign_extend(*va), len, Backing::System { pages: &phys, snooped: !*uswc }, f).map_err(|e| e.errno())?;
    }
    Ok(mem)
}

fn apply_op(heaps: &Heaps, g: &mut State, op: &PtOp) -> Result<(), Errno> {
    let State { store, tables, held, garbage, .. } = g;
    let vm = tables.as_mut().ok_or(EIO)?;
    match op {
        PtOp::Clear { va, pages } => {
            let len = pages.checked_mul(PAGE).ok_or(EINVAL)?;
            let (s, e) = (va >> PAGE_SHIFT, (va >> PAGE_SHIFT) + pages);
            // The room first: once the tables changed the memory must find its way to the garbage.
            garbage.try_reserve(touched(held, s, e)).map_err(|_| ENOMEM)?;
            vm.clear(store, sign_extend(*va), len).map_err(|e| e.errno())?;
            cut(held, s, e, garbage, vm.tlb_seq());
        }
        PtOp::Prt { va, pages, flags } => {
            let len = pages.checked_mul(PAGE).ok_or(EINVAL)?;
            let (f, _) = MapFlags::from_uapi(*flags);
            vm.map(store, sign_extend(*va), len, Backing::Prt, f).map_err(|e| e.errno())?;
            held.insert(va >> PAGE_SHIFT, Piece { pages: *pages, mem: None, first: 0 });
        }
        PtOp::Map { va, pages, first, .. } => {
            let mem = map_memory(heaps, vm, store, op)?;
            held.insert(va >> PAGE_SHIFT, Piece { pages: *pages, mem: Some(mem), first: *first });
        }
    }
    Ok(())
}

impl GpuVm for HwVm {
    /// Behind a deferred update it queues too (the order of one VM's updates holds for both calls).
    fn update(&self, ops: &[PtOp]) -> VmUpdate {
        self.update_after(ops, &[])
    }
    /// The answer comes when no other writer of this VM runs; the writes themselves are CPU stores (a large map is
    /// many uncached stores), so `fence` is None unless a dependency is pending.
    fn update_after(&self, ops: &[PtOp], deps: &[Arc<Fence>]) -> VmUpdate {
        let pending: Vec<&Arc<Fence>> = deps.iter().filter(|f| !fence::is_signaled(f)).collect();
        if pending.is_empty() {
            let (r, gone) = {
                let mut g = self.st.lock();
                if g.deferred.is_empty() {
                    let r = self.apply_all(&mut g, ops);
                    let gone = self.reclaim(&mut g);
                    (Some(r), gone)
                } else {
                    (None, Vec::new())
                }
            };
            drop(gone);
            if let Some(r) = r {
                return r;
            }
        }
        let mut copy = Vec::new();
        if copy.try_reserve_exact(ops.len()).is_err() {
            return VmUpdate { done: 0, error: Some(ENOMEM), fence: None };
        }
        copy.extend(ops.iter().cloned());
        let dep = (!pending.is_empty()).then(|| fence::merge(&pending));
        let reg = dep.as_ref().and_then(|d| fence::add_callback(d, Arc::new(Wake(self.me.clone()))));
        let seq = self.fence_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let done = Fence::new(self.fence_ctx, seq, "amdgpu", "vm");
        {
            let mut g = self.st.lock();
            if g.deferred.try_reserve(1).is_err() {
                return VmUpdate { done: 0, error: Some(ENOMEM), fence: None };
            }
            g.deferred.push_back(Deferred { ops: copy, dep, done: done.clone(), _reg: reg });
        }
        klog::event!(Debug, "gx", "hw.vm_deferred", ops = ops.len(), deps = pending.len());
        // A dependency that signaled before the entry was queued found nothing to run.
        if self.st.lock().deferred.front().is_some_and(|h| h.dep.as_ref().is_none_or(fence::is_signaled)) {
            self.pump();
        }
        VmUpdate { done: ops.len(), error: None, fence: Some(done) }
    }
    fn last_fault(&self) -> Option<VmFault> {
        *self.fault.lock()
    }
    fn as_any(&self) -> Option<&dyn core::any::Any> {
        Some(self)
    }
}

impl Drop for HwVm {
    /// `amdgpu_vm_fini`: no job holds the VM any more (each holds an `Arc`). Waiters for a deferred update must not
    /// hang, and must not hear that it ran: its fence ends with ESRCH, as an entity's killed jobs do
    /// (linux-6.6/drivers/gpu/drm/scheduler/sched_entity.c:168). The tables go, so nothing is reachable.
    fn drop(&mut self) {
        let g = self.st.get_mut();
        let deferred = core::mem::take(&mut g.deferred);
        if let Some(vm) = g.tables.take() {
            vm.destroy(&mut g.store);
        }
        for d in deferred {
            klog::event!(Debug, "gx", "hw.vm_update_dropped", ops = d.ops.len());
            d.done.signal_error(ESRCH as i32);
        }
        self.ids.mgr.lock().mgr.forget(self.owner);
        if let Err(e) = self.ids.pasids.lock().free(self.pasid) {
            klog::event!(Error, "gx", "hw.pasid_free_failed", pasid = self.pasid, why = alloc::format!("{e:?}").as_str());
        }
        self.live.fetch_sub(1, Ordering::AcqRel);
        klog::event!(Debug, "gx", "hw.vm_gone", pasid = self.pasid);
    }
}
