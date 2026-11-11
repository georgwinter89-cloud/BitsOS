//! DR-7: the GPU VM of one DRM file (Linux `amdgpu_fpriv.vm`, linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_vm.c):
//! the interval map of the client's GPU addresses, one bo_va per object the file has handles for (with the
//! mappings of that object), the PRT bo_va for mappings without memory, the VMID reservation flag, and the queue
//! of page-table jobs for the backend (`gpu::GpuVm`). GEM_VA (amdgpu_gem.c:664) and AMDGPU_VM (amdgpu_vm.c:2463)
//! end in `vm_ioctl`. The backend's VM is made at the first GEM_VA (Linux makes it at open; nothing a program sees
//! differs) and dropped at the last close.

use crate::fence::Fence;
use crate::gem::Bo;
use crate::gpu::{GpuDevice, GpuShared, GpuVm, PtOp};
use crate::uapi::*;
use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::SpinLock;
use linux::errno::{EINVAL, ENOENT, ENOMEM};
use vfs::Errno;

const PAGE_SHIFT: u32 = 12;
const PAGE_MASK: u64 = (1 << PAGE_SHIFT) - 1;
/// The canonical hole between the two halves and the mask into the 48-bit index space (amdgpu_gmc.h:35-45).
pub(crate) const HOLE_START: u64 = 0x0000_8000_0000_0000;
pub(crate) const HOLE_END: u64 = 0xffff_8000_0000_0000;
pub(crate) const HOLE_MASK: u64 = 0x0000_ffff_ffff_ffff;

/// amdgpu_gem.c:666-670.
pub(crate) const VALID_FLAGS: u32 = AMDGPU_VM_DELAY_UPDATE
    | AMDGPU_VM_PAGE_READABLE
    | AMDGPU_VM_PAGE_WRITEABLE
    | AMDGPU_VM_PAGE_EXECUTABLE
    | AMDGPU_VM_MTYPE_MASK
    | AMDGPU_VM_PAGE_NOALLOC;
pub(crate) const PRT_FLAGS: u32 = AMDGPU_VM_DELAY_UPDATE | AMDGPU_VM_PAGE_PRT;

/// The user part of the VM: `[bottom, top)` in the index space. Taken from the DEV_INFO the device reports, so
/// the kernel refuses exactly what it told libdrm's VA manager not to use (the captured kernel reserves 64 KiB at
/// the bottom and 4 MiB + 64 KiB at the top, Linux 6.6 2 MiB at both ends; amdgpu_gem.c:685-710).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub bottom: u64,
    pub top: u64,
}

impl Limits {
    pub fn of(d: &drm_amdgpu_info_device) -> Limits {
        let top = if d.high_va_max != 0 { d.high_va_max & HOLE_MASK } else { d.virtual_address_max };
        Limits { bottom: d.virtual_address_offset, top }
    }
}

/// Whose a mapping is: an object's bo_va (by the object's address) or the file's PRT bo_va.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Owner {
    Prt,
    Bo(usize),
}

/// `struct amdgpu_bo_va_mapping`: pages `[start, last]` (the key is the start), the byte offset into the object.
#[derive(Clone, Copy, Debug)]
struct VaMap {
    last: u64,
    offset: u64,
    flags: u32,
    owner: Owner,
}

/// `struct amdgpu_bo_va`: the object, how many handles of this file name it (`ref_count`), its mappings.
pub(crate) struct BoVa {
    pub(crate) bo: Arc<Bo>,
    handles: u64,
    maps: BTreeSet<u64>,
}

#[derive(Default)]
pub(crate) struct State {
    maps: BTreeMap<u64, VaMap>,
    pub(crate) bos: BTreeMap<usize, BoVa>,
    prt: BTreeSet<u64>,
    /// Jobs not yet handed to the backend (DELAY_UPDATE, or queued while another update ran).
    queue: VecDeque<PtOp>,
    /// Jobs the backend could not do for lack of memory last time; they run before `queue`.
    retry: VecDeque<PtOp>,
    /// `vm->last_update`.
    last_update: Option<Arc<Fence>>,
    /// bo_vas whose last handle went while there was no room for their unmap jobs (`handles == 0`); `flush`
    /// and the next `bo_open` of the object finish them.
    orphans: usize,
    /// A Clear the backend refused for a reason other than memory is at the head of `retry` (logged once).
    stuck: bool,
    released: bool,
}

/// One file's VM.
pub struct FileVm {
    pub(crate) st: SpinLock<State>,
    /// The backend's VM; its lock is the update lock (Linux: the root PD's reservation), so the jobs of this VM
    /// reach the backend in queue order. Never taken under `st`.
    backend: ksync::Mutex<Option<Arc<dyn GpuVm>>>,
    pub(crate) reserved_vmid: AtomicBool,
    /// Serialises this file's RESERVE/UNRESERVE (flag and device owner change together, as under Linux's
    /// `vm_manager.lock`).
    vmid: SpinLock<()>,
    /// DR-9: the VM root's reservation object (the file's `FileGem::vm_resv`): every CS of the file puts its
    /// fences there, and a Clear runs only after them (finding 8 of DR-8).
    root: Arc<crate::resv::Resv>,
}

impl Default for FileVm {
    fn default() -> Self {
        FileVm::new()
    }
}

pub(crate) fn key(bo: &Bo) -> usize {
    bo as *const Bo as usize
}

/// A mapping as tests and fdinfo see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MapInfo {
    pub va: u64,
    pub size: u64,
    pub offset: u64,
    pub flags: u32,
    pub prt: bool,
}

impl FileVm {
    pub fn new() -> FileVm {
        FileVm::with_root(crate::resv::Resv::new())
    }

    /// A VM whose root reservation object is `root` (DrmFile: the file's `vm_resv`).
    pub fn with_root(root: Arc<crate::resv::Resv>) -> FileVm {
        FileVm {
            st: SpinLock::named("drm.vm.state", State::default()),
            backend: ksync::Mutex::named("drm.vm.update", None),
            reserved_vmid: AtomicBool::new(false),
            vmid: SpinLock::named("drm.vm.vmid", ()),
            root,
        }
    }

    /// AMDGPU_VM_OP_RESERVE_VMID: Ok when this file holds the reserved VMID afterwards (also again, and also when
    /// two of its threads race); ENOENT while another file holds it.
    pub(crate) fn reserve_vmid(&self, shared: &GpuShared, me: u64) -> Result<(), Errno> {
        let r = {
            let _g = self.vmid.lock();
            if self.reserved_vmid.load(Ordering::Acquire) {
                return Ok(());
            }
            match shared.reserved_vmid_owner.compare_exchange(0, me, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => Ok(()),
                Err(o) if o == me => Ok(()),
                Err(o) => Err(o),
            }
            .inspect(|_| self.reserved_vmid.store(true, Ordering::Release))
        };
        match r {
            Ok(()) => {
                klog::event!(Debug, "drm", "vm.vmid_reserved", client = me);
                Ok(())
            }
            Err(holder) => {
                klog::event!(Debug, "drm", "vm.vmid_busy", client = me, holder = holder);
                Err(ENOENT)
            }
        }
    }

    /// AMDGPU_VM_OP_UNRESERVE_VMID (and the last close): the VMID goes back if this file holds it.
    pub(crate) fn unreserve_vmid(&self, shared: Option<&GpuShared>, me: u64) {
        let had = {
            let _g = self.vmid.lock();
            let had = self.reserved_vmid.swap(false, Ordering::AcqRel);
            if had && let Some(s) = shared {
                let _ = s.reserved_vmid_owner.compare_exchange(me, 0, Ordering::AcqRel, Ordering::Acquire);
            }
            had
        };
        if had {
            klog::event!(Debug, "drm", "vm.vmid_unreserved", client = me);
        }
    }

    /// `amdgpu_gem_object_open`: a handle of this file names `bo` (bo_va found or added).
    pub(crate) fn bo_open(&self, bo: &Arc<Bo>) {
        let k = key(bo);
        let mut spare = ksync::Spare::new();
        let mut may_grow = true;
        let old = loop {
            let mut g = self.st.lock();
            if g.released {
                return;
            }
            // A bo_va left without handles under memory pressure: Linux would have removed it at the close, so
            // the new handle starts without its mappings when there is room for their unmap jobs now (made
            // with the lock dropped, `ksync::spare`).
            let old = match g.bos.get(&k) {
                Some(v) if v.handles == 0 => match take_bo_va(&mut g, k, &mut spare) {
                    Err(()) if may_grow => {
                        drop(g);
                        may_grow = spare.grow().is_ok();
                        continue;
                    }
                    taken => {
                        g.orphans = g.orphans.saturating_sub(1);
                        taken.ok().flatten()
                    }
                },
                _ => None,
            };
            g.bos.entry(k).and_modify(|v| v.handles += 1).or_insert_with(|| BoVa { bo: bo.clone(), handles: 1, maps: BTreeSet::new() });
            break old;
        };
        drop(old);
    }

    /// `amdgpu_gem_object_close`: a handle went; at the last one the bo_va and its mappings go and the queued
    /// jobs run at once (amdgpu_vm_bo_del + amdgpu_vm_clear_freed, which also clears delayed unmaps). Called without the file's other locks.
    pub(crate) fn bo_close(&self, bo: &Bo) {
        let k = key(bo);
        let mut spare = ksync::Spare::new();
        let mut first = true;
        let queued = loop {
            let mut g = self.st.lock();
            let Some(v) = g.bos.get_mut(&k) else { return };
            if first {
                v.handles = v.handles.saturating_sub(1);
            }
            // (A later round: reopened while the room was made, the new handle keeps the bo_va.)
            if v.handles > 0 {
                return;
            }
            let n = v.maps.len();
            let pending = !g.queue.is_empty();
            match take_bo_va(&mut g, k, &mut spare) {
                Ok(v) => {
                    if !first {
                        g.orphans = g.orphans.saturating_sub(1);
                    }
                    drop(g);
                    // The bo_va's reference to the object goes outside the lock.
                    drop(v);
                    break n > 0 || pending;
                }
                Err(()) => {
                    // The queue's room is made with the lock dropped (`ksync::spare`). Until then the bo_va
                    // counts as an orphan, so a `bo_open` or a flush in between finds it as one. Without memory
                    // for the room it stays one: the mappings stay for now (their memory stays held, nothing is
                    // freed under the GPU); the next flush or the last close finishes the bo_va.
                    if first {
                        g.orphans += 1;
                        first = false;
                    }
                    drop(g);
                    if spare.grow().is_err() {
                        klog::event!(Warn, "drm", "vm.close_deferred", mappings = n; "no memory for the unmap jobs: the mappings go at the next update");
                        return;
                    }
                }
            }
        };
        if queued && let Err(e) = self.flush() {
            klog::event!(Warn, "drm", "vm.update_failed", errno = e, op = "close"; "page-table update at GEM_CLOSE failed; it runs again at the next update");
        }
    }

    /// The backend's VM, made at the first use.
    pub(crate) fn backend(&self, gpu: &dyn GpuDevice, client: u64) -> Result<(), Errno> {
        let mut b = self.backend.lock();
        if b.is_none() {
            match gpu.vm_create(client) {
                Ok(vm) => {
                    klog::event!(Debug, "drm", "vm.created");
                    *b = Some(vm);
                }
                Err(e) => {
                    klog::event!(Warn, "drm", "vm.create_failed", errno = e);
                    return Err(e);
                }
            }
        }
        Ok(())
    }

    /// Hands the queued jobs to the backend, in order (`amdgpu_vm_clear_freed` + `amdgpu_vm_bo_update` +
    /// `amdgpu_vm_update_pdes`): one pass over the jobs left from last time, then one over what is queued now;
    /// jobs queued while it runs are the next flush's (another thread's GEM_VAs cannot keep it here). Jobs refused
    /// for lack of memory stay for the next flush. Another refusal drops a Map or Prt job (the range then faults,
    /// as after Linux's failed update) but never a Clear: the backend may still reach the old memory, so the
    /// Clear stays at the head and every flush fails until it has run. CS (DR-8) calls this before it submits
    /// and fails with the error. DR-9: a batch with a Clear goes to the backend with the VM root's unsignaled
    /// fences as dependencies (`GpuVm::update_after`): the backend clears after the jobs queued before it, so an
    /// UNMAP never pulls memory from under a job that was submitted with it; nothing here waits.
    pub fn flush(&self) -> Result<(), Errno> {
        let b = self.backend.lock();
        let Some(vm) = b.as_ref() else { return Ok(()) };
        self.finish_orphans();
        for retry in [true, false] {
            let batch = {
                let mut g = self.st.lock();
                core::mem::take(if retry { &mut g.retry } else { &mut g.queue })
            };
            if !batch.is_empty() {
                self.run(vm.as_ref(), batch)?;
            }
        }
        Ok(())
    }

    fn run(&self, vm: &dyn GpuVm, mut batch: VecDeque<PtOp>) -> Result<(), Errno> {
        let ops = batch.make_contiguous();
        let deps = if ops.iter().any(|o| matches!(o, PtOp::Clear { .. })) { self.root.fences(crate::resv::Usage::Bookkeep) } else { alloc::vec::Vec::new() };
        let r = vm.update_after(ops, &deps);
        // The memory of finished jobs goes outside the state lock (the backend holds what it mapped).
        batch.drain(..r.done.min(batch.len()));
        let error = match r.error {
            None if !batch.is_empty() => Some(linux::errno::EIO),
            e => e,
        };
        let mut log_stuck = false;
        if let Some(e) = error.filter(|&e| e != ENOMEM)
            && let Some(op) = batch.front()
        {
            let (va, pages) = op.range();
            if matches!(op, PtOp::Clear { .. }) {
                log_stuck = true;
            } else {
                klog::event!(Error, "drm", "vm.job_dropped", va = klog::Hex(va), pages = pages, errno = e);
                drop(batch.pop_front());
            }
        }
        let first_stuck = {
            let mut g = self.st.lock();
            if let Some(f) = r.fence {
                g.last_update = Some(f);
            }
            let first = log_stuck && !g.stuck;
            g.stuck = log_stuck;
            if !batch.is_empty() {
                // `retry` was taken (or was empty) before this batch ran.
                g.retry = batch;
            }
            first
        };
        if first_stuck && let Some(e) = error {
            klog::event!(Error, "drm", "vm.clear_stuck", errno = e; "the backend refused to clear a range: the VM's updates fail until it has");
        }
        match error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Finishes the bo_vas that `bo_close` left without room for their jobs (one at a time: each object's last
    /// reference may go outside the lock).
    fn finish_orphans(&self) {
        let mut spare = ksync::Spare::new();
        loop {
            let taken = {
                let mut g = self.st.lock();
                if g.orphans == 0 {
                    return;
                }
                let Some(k) = g.bos.iter().find(|(_, v)| v.handles == 0).map(|(&k, _)| k) else {
                    g.orphans = 0;
                    return;
                };
                let taken = take_bo_va(&mut g, k, &mut spare);
                if taken.is_ok() {
                    g.orphans -= 1;
                }
                taken
            };
            match taken {
                Ok(v) => drop(v),
                // The queue's room is made here, with the lock dropped; without memory the orphans stay.
                Err(()) => {
                    if spare.grow().is_err() {
                        return;
                    }
                }
            }
        }
    }

    /// Host tests: a backend of their own (one that refuses jobs, or queues more while it runs).
    #[cfg(test)]
    pub(crate) fn set_backend_for_test(&self, vm: Arc<dyn GpuVm>) {
        *self.backend.lock() = Some(vm);
    }

    /// The backend's VM (None before the first GEM_VA and after the last close).
    pub fn backend_vm(&self) -> Option<Arc<dyn GpuVm>> {
        self.backend.lock().clone()
    }

    /// `vm->last_update`: the fence of the last page-table write (None: written).
    pub fn last_update(&self) -> Option<Arc<Fence>> {
        self.st.lock().last_update.clone()
    }

    /// Queued jobs not yet run (DELAY_UPDATE).
    pub fn queued(&self) -> usize {
        let g = self.st.lock();
        g.queue.len() + g.retry.len()
    }

    /// The mappings, by address.
    pub fn mappings(&self) -> alloc::vec::Vec<MapInfo> {
        let g = self.st.lock();
        g.maps
            .iter()
            .map(|(&s, m)| MapInfo { va: s << PAGE_SHIFT, size: (m.last - s + 1) << PAGE_SHIFT, offset: m.offset, flags: m.flags, prt: m.owner == Owner::Prt })
            .collect()
    }

    /// bo_vas of the file (objects it has handles for).
    pub fn bo_vas(&self) -> usize {
        self.st.lock().bos.len()
    }

    pub fn vmid_reserved(&self) -> bool {
        self.reserved_vmid.load(Ordering::Acquire)
    }

    /// The last close (`amdgpu_vm_fini`): everything goes, the backend's VM last (it lets go of the memory its
    /// mappings hold); the reserved VMID is given back (amdgpu_vm.c:2378-2384).
    pub(crate) fn release(&self, shared: Option<&GpuShared>, client: u64) {
        let st = {
            let mut g = self.st.lock();
            let st = core::mem::take(&mut *g);
            g.released = true;
            st
        };
        let vm = self.backend.lock().take();
        self.unreserve_vmid(shared, client);
        if !st.maps.is_empty() || vm.is_some() {
            klog::event!(Debug, "drm", "vm.released", mappings = st.maps.len(), bo_vas = st.bos.len(), queued = st.queue.len() + st.retry.len());
        }
        drop(st);
        drop(vm);
    }
}

// ---- the interval map (amdgpu_vm.c bo_map, bo_unmap, bo_clear_mappings)

fn owner_set(g: &mut State, o: Owner) -> Option<&mut BTreeSet<u64>> {
    match o {
        Owner::Prt => Some(&mut g.prt),
        Owner::Bo(k) => g.bos.get_mut(&k).map(|v| &mut v.maps),
    }
}

/// The job queue grows with `st` dropped: under a spin lock the heap can neither reclaim nor fall back to single
/// frames, and `try_reserve` there answered ENOMEM although memory was there (see `ksync::spare`).
type Room = ksync::Spare<VecDeque<PtOp>>;

/// `vm.st` locked, `check` passed and room for `n` more jobs in the queue; `check` runs again whenever the
/// lock was dropped for the room.
fn lock_with_room<'a>(vm: &'a FileVm, room: &mut Room, n: usize, check: impl Fn(&State) -> Result<(), Errno>) -> Result<ksync::SpinLockGuard<'a, State>, Errno> {
    loop {
        let mut g = vm.st.lock();
        check(&g)?;
        if room.fits(&mut g.queue, n) {
            return Ok(g);
        }
        drop(g);
        room.grow().map_err(|_| ENOMEM)?;
    }
}

/// The handle closed since the lookup (amdgpu_vm_bo_find: ENOENT).
fn bo_known(g: &State, owner: Owner) -> Result<(), Errno> {
    match owner {
        Owner::Bo(k) if !g.bos.contains_key(&k) => Err(ENOENT),
        _ => Ok(()),
    }
}

/// Removes the bo_va of `k` and queues the unmap of its mappings; Err without room for the jobs (the caller
/// lets `room` grow with the lock dropped, or leaves the bo_va).
fn take_bo_va(g: &mut State, k: usize, room: &mut Room) -> Result<Option<BoVa>, ()> {
    let n = g.bos.get(&k).map_or(0, |v| v.maps.len());
    if n > 0 && !room.fits(&mut g.queue, n) {
        return Err(());
    }
    let Some(v) = g.bos.remove(&k) else { return Ok(None) };
    for &s in &v.maps {
        if let Some(m) = g.maps.remove(&s) {
            g.queue.push_back(PtOp::Clear { va: s << PAGE_SHIFT, pages: m.last - s + 1 });
        }
    }
    Ok(Some(v))
}

fn insert(g: &mut State, start: u64, m: VaMap) {
    if let Some(set) = owner_set(g, m.owner) {
        set.insert(start);
    }
    g.maps.insert(start, m);
}

fn remove(g: &mut State, start: u64) -> Option<VaMap> {
    let m = g.maps.remove(&start)?;
    if let Some(set) = owner_set(g, m.owner) {
        set.remove(&start);
    }
    Some(m)
}

/// The first mapping that overlaps pages `[s, e]`.
fn first_overlap(g: &State, s: u64, e: u64) -> Option<u64> {
    if let Some((&ms, m)) = g.maps.range(..s).next_back()
        && m.last >= s
    {
        return Some(ms);
    }
    g.maps.range(s..=e).next().map(|(&ms, _)| ms)
}

/// Removes every translation in pages `[s, e]`, keeping the parts of mappings outside it (amdgpu_vm.c:1699).
/// True when anything was there.
fn clear_range(g: &mut State, s: u64, e: u64) -> bool {
    let mut any = false;
    while let Some(ms) = first_overlap(g, s, e) {
        let Some(m) = remove(g, ms) else { break };
        any = true;
        if ms < s {
            insert(g, ms, VaMap { last: s - 1, ..m });
        }
        if m.last > e {
            let after = e + 1;
            insert(g, after, VaMap { offset: m.offset + ((after - ms) << PAGE_SHIFT), ..m });
        }
    }
    any
}

/// MAP and REPLACE after the ioctl's checks: `va` in the index space.
#[allow(clippy::too_many_arguments)]
pub(crate) fn map(vm: &FileVm, owner: Owner, bo: Option<&Arc<Bo>>, va: u64, offset: u64, size: u64, flags: u32, replace: bool) -> Result<(), Errno> {
    // amdgpu_vm.c:1527-1536 / :1594-1602.
    if (va | offset | size) & PAGE_MASK != 0 {
        return Err(EINVAL);
    }
    let end = va.checked_add(size).filter(|&e| e > va).ok_or(EINVAL)?;
    let bo_end = offset.checked_add(size).filter(|&e| e > offset).ok_or(EINVAL)?;
    if bo.is_some_and(|b| bo_end > b.size()) {
        return Err(EINVAL);
    }
    let (s, e) = (va >> PAGE_SHIFT, (end - 1) >> PAGE_SHIFT);
    let op_flags = flags & !AMDGPU_VM_DELAY_UPDATE;
    let op = match bo {
        Some(b) => PtOp::Map {
            va,
            pages: e - s + 1,
            mem: b.memory().clone(),
            first: offset >> PAGE_SHIFT,
            place: b.place(),
            uswc: b.flags() & AMDGPU_GEM_CREATE_CPU_GTT_USWC as u64 != 0,
            flags: op_flags,
        },
        None => PtOp::Prt { va, pages: e - s + 1, flags: op_flags },
    };
    let mut room = Room::new();
    let mut g = lock_with_room(vm, &mut room, 2, |g| bo_known(g, owner))?;
    if replace {
        if clear_range(&mut g, s, e) {
            g.queue.push_back(PtOp::Clear { va, pages: e - s + 1 });
        }
    } else if let Some(other) = first_overlap(&g, s, e) {
        let o = g.maps.get(&other).map(|m| m.last).unwrap_or(other);
        drop(g);
        klog::event!(Debug, "drm", "vm.map_overlap", va = klog::Hex(va), size = size, other = klog::Hex(other << PAGE_SHIFT), other_end = klog::Hex((o + 1) << PAGE_SHIFT));
        return Err(EINVAL);
    }
    insert(&mut g, s, VaMap { last: e, offset, flags: op_flags, owner });
    g.queue.push_back(op);
    Ok(())
}

/// UNMAP: the whole mapping of this bo_va that starts at the page of `va` (amdgpu_vm.c:1645).
pub(crate) fn unmap(vm: &FileVm, owner: Owner, va: u64) -> Result<(), Errno> {
    let s = va >> PAGE_SHIFT;
    let mut room = Room::new();
    let mut g = lock_with_room(vm, &mut room, 1, |g| {
        bo_known(g, owner)?;
        match g.maps.get(&s) {
            Some(m) if m.owner == owner => Ok(()),
            _ => Err(ENOENT),
        }
    })?;
    let m = remove(&mut g, s).ok_or(ENOENT)?;
    g.queue.push_back(PtOp::Clear { va: s << PAGE_SHIFT, pages: m.last - s + 1 });
    Ok(())
}

/// CLEAR: nothing mapped in `[va, va + size)` any more, whoever's (amdgpu_vm.c:1699). As on CachyOS's kernel
/// (the ABI program shows it) an unaligned range or size 0 is EINVAL; Linux 6.6 took the pages of the first and
/// the last byte unchecked.
pub(crate) fn clear(vm: &FileVm, va: u64, size: u64) -> Result<(), Errno> {
    if (va | size) & PAGE_MASK != 0 {
        return Err(EINVAL);
    }
    let end = va.checked_add(size).filter(|&e| e > va).ok_or(EINVAL)?;
    let (s, e) = (va >> PAGE_SHIFT, (end - 1) >> PAGE_SHIFT);
    let mut room = Room::new();
    let mut g = lock_with_room(vm, &mut room, 1, |_| Ok(()))?;
    if clear_range(&mut g, s, e) {
        g.queue.push_back(PtOp::Clear { va: s << PAGE_SHIFT, pages: e - s + 1 });
    }
    Ok(())
}
