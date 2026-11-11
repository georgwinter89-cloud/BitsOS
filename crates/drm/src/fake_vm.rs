//! DR-7: the fake GPU's client VMs. A job's ops build real GFX 11 page tables in heap memory with GR's VM core
//! (`amdgpu_fmt::vm`, the CPU path, as GX-8a does on the chip), and the fake keeps the memory of
//! every mapped page for as long as a translation reaches it. Without physical addresses (host tests have no
//! frames) only the translation map is kept. TLB flushes are immediate, so freed tables go back at once.
//!
//! DR-9: an update with dependencies (`update_after`: an UNMAP behind the VM's pending jobs) waits in the VM's
//! own queue, and every later update behind it, as the SDMA page-table jobs of Linux do; the first fence of the
//! queue's head that signals runs what became ready. A job's page fault is kept as the VM's last fault
//! (AMDGPU_INFO_GPUVM_FAULT).

use crate::dma_buf::Exporter;
use crate::fence::{self, Fence, FenceCb, Registration};
use crate::gpu::{BoPlace, GpuVm, PtOp, VmFault, VmUpdate};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use amdgpu_fmt::addr::{Carveout, sign_extend};
use amdgpu_fmt::pte::{MapFlags, Target};
use amdgpu_fmt::store::{ENTRIES, TableAddr, TableRead, TableStore};
use amdgpu_fmt::vm::{Backing, Vm};
use amdgpu_fmt::walk::{self, Walk};
use core::sync::atomic::{AtomicU64, Ordering};
use ksync::SpinLock;
use linux::errno::{EIO, ENOMEM, ESRCH};

const PAGE_SHIFT: u32 = 12;
/// Where the fake's tables "live" (above any RAM a test machine has, inside 48 bits; never touched).
const TABLE_BASE: u64 = 0xe000_0000_0000;
/// The fake's carve-out as the tables see it: its "VRAM" objects are system pages (DR-6 decision 1), so no local
/// mapping is ever made; the VM core still wants a carve-out.
const LOCAL_BASE: u64 = 0xd000_0000_0000;

/// Page tables in heap memory.
#[derive(Default)]
struct HeapTables {
    tables: BTreeMap<u64, Box<[u64]>>,
    free: Vec<u64>,
    next: u64,
}

impl TableRead for HeapTables {
    fn read(&self, table: u64, index: usize) -> Option<u64> {
        self.tables.get(&table).and_then(|t| t.get(index).copied())
    }
}

impl TableStore for HeapTables {
    fn alloc(&mut self) -> Option<TableAddr> {
        let mut v = Vec::new();
        v.try_reserve_exact(ENTRIES).ok()?;
        v.resize(ENTRIES, 0);
        let phys = match self.free.pop() {
            Some(p) => p,
            None => {
                let p = TABLE_BASE.checked_add(self.next.checked_mul(4096)?)?;
                if p >> 48 != 0 {
                    return None;
                }
                self.next += 1;
                p
            }
        };
        self.tables.insert(phys, v.into_boxed_slice());
        Some(TableAddr { phys, target: Target::System { snooped: true } })
    }
    /// Heap memory has no fixed bound; an `alloc` that finds none fails on its own (ENOMEM).
    fn available(&self) -> usize {
        usize::MAX / 2
    }
    fn free(&mut self, table: TableAddr) {
        if self.tables.remove(&table.phys).is_some() && self.free.try_reserve(1).is_ok() {
            self.free.push(table.phys);
        }
    }
    fn write(&mut self, table: u64, index: usize, value: u64) {
        if let Some(e) = self.tables.get_mut(&table).and_then(|t| t.get_mut(index)) {
            *e = value;
        }
    }
}

/// What a range of the fake VM translates to.
#[derive(Clone)]
pub enum Held {
    Mem { mem: Arc<dyn Exporter>, first: u64 },
    Prt,
}

#[derive(Clone)]
struct Piece {
    pages: u64,
    held: Held,
    flags: u32,
}

struct State {
    store: HeapTables,
    tables: Option<Vm>,
    /// Start page -> piece: what the GPU can reach (keeps the memory).
    held: BTreeMap<u64, Piece>,
    /// Map ops whose memory had no physical addresses (host tests): in `held` only.
    untabled: u64,
    /// DR-9: updates waiting for their dependencies (or for an earlier waiting update), in order.
    deferred: VecDeque<Deferred>,
}

/// One `update_after` that could not run at once.
struct Deferred {
    ops: Vec<PtOp>,
    /// None: waits only for the updates before it.
    dep: Option<Arc<Fence>>,
    done: Arc<Fence>,
    _reg: Option<Registration>,
}

/// Runs the queue when a dependency signals (the VM may be gone by then).
struct Wake(Weak<FakeVm>);

impl FenceCb for Wake {
    fn call(&self) {
        if let Some(vm) = self.0.upgrade() {
            vm.drain();
        }
    }
}

/// A translation of the fake VM.
pub struct Translation {
    pub held: Held,
    /// The page inside the memory (for `Held::Mem`).
    pub page: u64,
    pub flags: u32,
}

/// What the page tables say about a VA (the GFX 11 walk), when the fake built them.
#[derive(Debug, PartialEq, Eq)]
pub enum TableWalk {
    Page { phys: u64, read: bool, write: bool, exec: bool, snooped: bool },
    Prt,
    Hole,
    NoTables,
    Bad,
}

pub struct FakeVm {
    me: Weak<FakeVm>,
    st: SpinLock<State>,
    updates: AtomicU64,
    /// The fence timeline of deferred updates.
    fence_ctx: u64,
    fence_seq: AtomicU64,
    /// The last page fault of a job on this VM.
    fault: SpinLock<Option<VmFault>>,
}

/// Live fake VMs (tests check the balance).
pub(crate) static LIVE_VMS: AtomicU64 = AtomicU64::new(0);

impl FakeVm {
    pub(crate) fn new(carveout: u64) -> Result<Arc<FakeVm>, vfs::Errno> {
        let mut store = HeapTables::default();
        let size = carveout.max(4096) & !0xfff;
        let local = Carveout::new(0, LOCAL_BASE, size).ok_or(ENOMEM)?;
        let vm = Vm::new(&mut store, local).map_err(|e| e.errno())?;
        LIVE_VMS.fetch_add(1, Ordering::Relaxed);
        let st = State { store, tables: Some(vm), held: BTreeMap::new(), untabled: 0, deferred: VecDeque::new() };
        Ok(Arc::new_cyclic(|me| FakeVm {
            me: me.clone(),
            st: SpinLock::named("drm.fake.vm", st),
            updates: AtomicU64::new(0),
            fence_ctx: fence::context_alloc(1),
            fence_seq: AtomicU64::new(0),
            fault: SpinLock::named("drm.fake.vm_fault", None),
        }))
    }

    /// Updates waiting in the VM's queue.
    pub fn deferred(&self) -> usize {
        self.st.lock().deferred.len()
    }

    /// DR-9: a job of this VM faulted at `va` (index space): kept as the last fault (`amdgpu_vm_update_fault_cache`).
    pub(crate) fn note_fault(&self, f: VmFault) {
        *self.fault.lock() = Some(f);
    }

    /// Runs the queue's head while its dependency signaled, each update with its ops in one piece under the lock
    /// (two threads draining at once still apply in queue order); its fence signals after (EIO if an op failed).
    fn drain(&self) {
        loop {
            let mut gone = Vec::new();
            let (d, error) = {
                let mut g = self.st.lock();
                match g.deferred.front() {
                    Some(h) if h.dep.as_ref().is_none_or(fence::is_signaled) => {}
                    _ => return,
                }
                let Some(d) = g.deferred.pop_front() else { return };
                let mut error = None;
                for op in &d.ops {
                    if let Err(e) = apply(&mut g, op, &mut gone) {
                        error = Some(e);
                        break;
                    }
                }
                (d, error)
            };
            drop(gone);
            match error {
                Some(e) => {
                    klog::event!(Error, "drm", "fake.vm_deferred_failed", ops = d.ops.len(), errno = e; "a deferred page-table update failed: its fence ends with the error");
                    d.done.signal_error(e as i32);
                }
                None => {
                    d.done.signal();
                }
            }
        }
    }

    /// The translation of `va` (index space or canonical).
    pub fn translate(&self, va: u64) -> Option<Translation> {
        let p = (va & 0x0000_ffff_ffff_ffff) >> PAGE_SHIFT;
        let g = self.st.lock();
        let (&s, piece) = g.held.range(..=p).next_back()?;
        if p >= s + piece.pages {
            return None;
        }
        let page = match &piece.held {
            Held::Mem { first, .. } => first + (p - s),
            Held::Prt => 0,
        };
        Some(Translation { held: piece.held.clone(), page, flags: piece.flags })
    }

    /// Walks the fake's GFX 11 tables at `va`.
    pub fn walk(&self, va: u64) -> TableWalk {
        let g = self.st.lock();
        let Some(vm) = g.tables.as_ref() else { return TableWalk::NoTables };
        match walk::walk(&g.store, vm.root().phys, sign_extend(va & 0x0000_ffff_ffff_ffff)) {
            Ok(Walk::Page(l)) => TableWalk::Page {
                phys: l.phys,
                read: l.flags.read,
                write: l.flags.write,
                exec: l.flags.exec,
                snooped: matches!(l.target, Target::System { snooped: true }),
            },
            Ok(Walk::Prt { .. }) => TableWalk::Prt,
            Ok(Walk::Hole { .. }) => TableWalk::Hole,
            Err(_) => TableWalk::Bad,
        }
    }

    /// Ranges held (mappings as the GPU sees them, pieces after splits).
    pub fn pieces(&self) -> usize {
        self.st.lock().held.len()
    }
    /// Tables the fake has now.
    pub fn tables(&self) -> usize {
        self.st.lock().store.tables.len()
    }
    pub fn untabled(&self) -> u64 {
        self.st.lock().untabled
    }
    /// Update calls so far.
    pub fn updates(&self) -> u64 {
        self.updates.load(Ordering::Relaxed)
    }
}

impl Drop for FakeVm {
    fn drop(&mut self) {
        let g = self.st.get_mut();
        // Waiters for a deferred update (a CS's last_update) must not hang: the tables go, so nothing is reachable.
        let deferred = core::mem::take(&mut g.deferred);
        if let Some(vm) = g.tables.take() {
            vm.destroy(&mut g.store);
        }
        for d in deferred {
            d.done.signal_error(ESRCH as i32);
        }
        LIVE_VMS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Removes `[s, e)` (pages) from the held map, keeping what sticks out; the removed memory goes to `gone`.
fn cut(held: &mut BTreeMap<u64, Piece>, s: u64, e: u64, gone: &mut Vec<Piece>) {
    loop {
        let first = held.range(..s).next_back().filter(|(k, p)| **k + p.pages > s).map(|(k, _)| *k);
        let Some(k) = first.or_else(|| held.range(s..e).next().map(|(k, _)| *k)) else { break };
        let Some(p) = held.remove(&k) else { break };
        let end = k + p.pages;
        if k < s {
            held.insert(k, Piece { pages: s - k, ..p.clone() });
        }
        if end > e {
            let held_after = match &p.held {
                Held::Mem { mem, first } => Held::Mem { mem: mem.clone(), first: first + (e - k) },
                Held::Prt => Held::Prt,
            };
            held.insert(e, Piece { pages: end - e, held: held_after, flags: p.flags });
        }
        gone.push(p);
    }
}

/// `ops` in order under one hold of the state, up to the first that fails.
fn apply_all(g: &mut State, ops: &[PtOp], gone: &mut Vec<Piece>) -> VmUpdate {
    let mut done = 0;
    let mut error = None;
    for op in ops {
        if let Err(e) = apply(g, op, gone) {
            let (va, pages) = op.range();
            error = Some(e);
            klog::event!(Warn, "drm", "fake.vm_update_failed", va = klog::Hex(va), pages = pages, errno = e);
            break;
        }
        done += 1;
    }
    VmUpdate { done, error, fence: None }
}

fn apply(g: &mut State, op: &PtOp, gone: &mut Vec<Piece>) -> Result<(), vfs::Errno> {
    let State { store, tables, held, untabled, .. } = g;
    let vm = tables.as_mut().ok_or(EIO)?;
    match op {
        PtOp::Clear { va, pages } => {
            let len = pages.checked_mul(4096).ok_or(EIO)?;
            vm.clear(store, sign_extend(*va), len).map_err(|e| e.errno())?;
            cut(held, va >> PAGE_SHIFT, (va >> PAGE_SHIFT) + pages, gone);
        }
        PtOp::Prt { va, pages, flags } => {
            let len = pages.checked_mul(4096).ok_or(EIO)?;
            let (f, _) = MapFlags::from_uapi(*flags);
            vm.map(store, sign_extend(*va), len, Backing::Prt, f).map_err(|e| e.errno())?;
            if let Some(old) = held.insert(va >> PAGE_SHIFT, Piece { pages: *pages, held: Held::Prt, flags: *flags }) {
                gone.push(old);
            }
        }
        PtOp::Map { va, pages, mem, first, place, uswc, flags } => {
            let len = pages.checked_mul(4096).ok_or(EIO)?;
            let n = usize::try_from(*pages).map_err(|_| ENOMEM)?;
            let mut phys = Vec::new();
            phys.try_reserve_exact(n).map_err(|_| ENOMEM)?;
            let mut all = true;
            for i in 0..*pages {
                match mem.page_phys(first + i) {
                    Some(p) => phys.push(p),
                    None => {
                        all = false;
                        break;
                    }
                }
            }
            if all {
                // amdgpu_ttm_tt_pde_flags: cached system pages are snooped, USWC and the carve-out are not.
                let snooped = !*uswc && *place != Some(BoPlace::Vram);
                let (f, _) = MapFlags::from_uapi(*flags);
                vm.map(store, sign_extend(*va), len, Backing::System { pages: &phys, snooped }, f).map_err(|e| e.errno())?;
            } else {
                *untabled += 1;
            }
            if let Some(old) = held.insert(va >> PAGE_SHIFT, Piece { pages: *pages, held: Held::Mem { mem: mem.clone(), first: *first }, flags: *flags }) {
                gone.push(old);
            }
        }
    }
    // The fake's TLB flush is immediate: unlinked tables go back now.
    let seq = vm.tlb_seq();
    vm.reclaim(store, seq);
    Ok(())
}

impl GpuVm for FakeVm {
    /// Behind a deferred update it queues too (the order of one VM's updates holds for both calls).
    fn update(&self, ops: &[PtOp]) -> VmUpdate {
        self.update_after(ops, &[])
    }
    fn update_after(&self, ops: &[PtOp], deps: &[Arc<Fence>]) -> VmUpdate {
        let pending: Vec<&Arc<Fence>> = deps.iter().filter(|f| !fence::is_signaled(f)).collect();
        if pending.is_empty() {
            // The look at the queue and the update in one hold: no other thread's deferred entry can come
            // between them, so this update never overtakes one queued before it.
            let mut gone = Vec::new();
            let r = {
                let mut g = self.st.lock();
                g.deferred.is_empty().then(|| apply_all(&mut g, ops, &mut gone))
            };
            // Memory the tables no longer reach goes outside the lock.
            drop(gone);
            if let Some(r) = r {
                self.updates.fetch_add(1, Ordering::Relaxed);
                return r;
            }
        }
        self.updates.fetch_add(1, Ordering::Relaxed);
        let mut copy = Vec::new();
        if copy.try_reserve_exact(ops.len()).is_err() {
            return VmUpdate { done: 0, error: Some(ENOMEM), fence: None };
        }
        copy.extend(ops.iter().cloned());
        let dep = (!pending.is_empty()).then(|| fence::merge(&pending));
        let reg = dep.as_ref().and_then(|d| fence::add_callback(d, Arc::new(Wake(self.me.clone()))));
        let seq = self.fence_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let done = Fence::new(self.fence_ctx, seq, "fake", "vm");
        {
            let mut g = self.st.lock();
            if g.deferred.try_reserve(1).is_err() {
                return VmUpdate { done: 0, error: Some(ENOMEM), fence: None };
            }
            g.deferred.push_back(Deferred { ops: copy, dep, done: done.clone(), _reg: reg });
        }
        klog::event!(Debug, "drm", "fake.vm_deferred", ops = ops.len(), deps = pending.len());
        // A dependency that signaled before the entry was queued found nothing to run.
        self.drain();
        VmUpdate { done: ops.len(), error: None, fence: Some(done) }
    }
    fn last_fault(&self) -> Option<VmFault> {
        *self.fault.lock()
    }
    fn as_fake(&self) -> Option<&FakeVm> {
        Some(self)
    }
}
