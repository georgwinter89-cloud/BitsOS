//! A process's address space (MM-5a, MM-5b): its own PML4 with the kernel half shared, the VMA tree with Linux's
//! `mmap`/`munmap`/`mprotect`/`mremap`/`madvise`/`mincore`/`msync`/`brk` semantics (`linux`), pages filled on the
//! first touch (`Fill::OnFault`, through `handle_user_fault`) or at map time (`Fill::Eager`, for callers that do
//! not route user page faults here), and user copies that fill the pages they touch instead of failing.
//!
//! MM-5b: copy-on-write for fork (`fork.rs`, share counts in `share`), strict commit (`commit`: private writable
//! memory is charged when promised, so a fault never meets an empty pool), shared anonymous memory as a nameless
//! tmpfs file (`shm.rs`), device mappings with a fault hook and WC/UC pages (`device`, `devmap.rs`), `madvise`
//! and `mincore` (`advise.rs`), `mremap` (`remap.rs`), per-process numbers (`stats.rs`).
//!
//! Frames: a mapped frame is either the program's own (`Kind::User`: anonymous memory and private copies; after
//! a fork shared read-only with a count in `share`, freed by its last holder), a page-cache frame pinned for a
//! file VMA (`MapBacking::get_page`/`put_page`), or a device's frame (never freed here). File mappings (FS-5): a
//! fault asks the backing for the page-cache frame: MAP_SHARED maps it with the VMA's rights, MAP_PRIVATE maps it
//! read-only and copies it into a private frame on the first write. Removing an entry hands the frame back once
//! the shootdown was confirmed; an unconfirmed one keeps it in `deferred` until the drop. So munmap and close
//! (and unlink) may come in any order.
//!
//! Locking: one sleeping `ksync::Mutex` per address space ("mm.as"; frames and heap are taken under it, and the
//! page cache's `vfs.inode.data`/`vfs.map.pins` and a device's fault hook from the fault path: mm.as is outside
//! them). The user copies hold it while they copy, so a concurrent `munmap` cannot free a frame under a copy.
//! msync's write-back and the creation of a shared anonymous object run without it. Nothing here runs in
//! interrupt context: user faults come back to the process's thread through `UserContext::execute`.
//!
//! TLB (MM-6): `hal::paging::unmap_user`/`protect_user` shoot the old translations down on every CPU that has
//! this root loaded and hand frames and emptied tables back only after every such CPU answered. A CPU that does
//! not answer at all leaves its frames in `deferred` (freed at the drop, after the drop's own shootdown) and an
//! event. The drop makes every CPU that still has the root loaded (lazily) leave it before anything is freed.
//! An unconfirmed `mprotect` downgrade returns success, as on Linux (which waits without a bound instead): the
//! IPI stays pending in the silent CPU's local APIC and flushes before that CPU runs user code again.

mod advise;
pub(crate) mod counters;
mod devmap;
pub(crate) mod fault;
mod fork;
mod remap;
pub mod rmap;
mod shm;
mod stats;

pub use stats::{MemStats, PageUsage, PSS_SHIFT};

use counters::{Class, Counters};

use crate::commit;
use crate::linux::{BrkPlan, Errno, MmapPlan, Space, MAP_ANONYMOUS, MAP_PRIVATE};
use crate::vma::{FileRef, Prot, Vma, VmaKind};
use crate::{share, MmError, MMAP_TOP, PAGE};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Deref;
use hal::paging::{self, PageFlags, Released};
use ksync::Mutex;
use vfs::mmap::{MapBacking, Mapped};

/// When pages get their frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fill {
    /// At map time (and when a range becomes accessible): for callers whose user faults do not reach
    /// `handle_user_fault`. PROT_NONE ranges stay empty. A fork of such a space copies instead of sharing.
    Eager,
    /// On the first touch (`handle_user_fault`, user copies).
    OnFault,
}

pub const MS_ASYNC: u64 = 1;
pub const MS_INVALIDATE: u64 = 2;
pub const MS_SYNC: u64 = 4;

/// The memory-type bits of a 4 KiB leaf (kept when rights change: a device page stays WC/UC).
const CACHE_BITS: u64 = PageFlags::PWT | PageFlags::PCD | PageFlags::PAT_4K;

klog::counter!(DEFERRED_FRAMES, "mm.as.frames_deferred");
klog::counter!(pub(crate) UNCONFIRMED, "mm.as.shootdowns_unconfirmed");
klog::counter!(pub(crate) AS_LEAKED, "mm.as.tables_leaked");
klog::counter!(FRAMES_LOST, "mm.as.frames_lost");

/// A frame or table whose shootdown a CPU did not confirm, freed at the drop.
enum Deferred {
    Frame(u64),
    Table(u64),
    /// A file mapping's page: handed back to its backing (which the entry keeps alive) at the drop.
    FilePage(u64, Arc<dyn MapBacking>),
}

/// Who owns the frames of a range that is being unmapped.
#[derive(Clone)]
pub(crate) enum Owner {
    /// The program's own frames (share-counted).
    Anon,
    /// Page-cache frames go back to the backing; a frame it does not know is a private copy (the program's).
    /// The flag: shared anonymous memory (its frames count as RssShmem, a file's as RssFile).
    File(Arc<dyn MapBacking>, bool),
    /// A device's frames: never freed here.
    Device,
}

/// The file and device parts of a range: (start, end, owner) per VMA, collected before the tree changes.
pub(crate) type Segs = Vec<(u64, u64, Owner)>;

pub(crate) struct Inner {
    pub(crate) space: Space,
    pub(crate) fill: Fill,
    deferred: Vec<Deferred>,
    /// Frames lost because `deferred` had no room (counted, never reused).
    lost: u64,
    /// This space's own commit limit in pages (YK-1's per-process `PR_MEM_LIMIT` hook); None: only the system's.
    limit: Option<u64>,
    /// ktest hook: a fork of this space fails after mapping this many pages in the child (the late-failure path).
    fork_fail_after: Option<u64>,
}

impl Inner {
    fn new(fill: Fill) -> Inner {
        Inner { space: Space::new(MMAP_TOP), fill, deferred: Vec::new(), lost: 0, limit: None, fork_fail_after: None }
    }

    /// Charges `pages` more (`pending` already charged in this call) against this space's limit and the system's.
    pub(crate) fn charge(&self, pages: u64, pending: u64) -> Result<(), Errno> {
        if pages == 0 {
            return Ok(());
        }
        if let Some(l) = self.limit
            && self.space.committed.saturating_add(pending).saturating_add(pages) > l
        {
            commit::refused_by_space(pages);
            return Err(Errno::NoMem);
        }
        if commit::charge(pages) { Ok(()) } else { Err(Errno::NoMem) }
    }

    /// After a tree change that was charged `charged` pages beforehand: the system gets back what the space no
    /// longer holds (unmapped accounted VMAs, a charge the change did not use).
    pub(crate) fn settle(&self, before: u64, charged: u64) {
        let after = self.space.committed;
        let expected = before + charged;
        if after < expected {
            commit::uncharge(expected - after);
        } else if after > expected {
            commit::force(after - expected);
        }
    }
}

/// One process's address space. The kernel half is shared with every other root. The state lives in an
/// `Arc<AsCore>` (`Deref`), so device mappings can find the spaces that map them.
pub struct AddressSpace {
    core: Arc<AsCore>,
}

impl Deref for AddressSpace {
    type Target = AsCore;
    fn deref(&self) -> &AsCore {
        &self.core
    }
}

/// The shared part of an `AddressSpace` (every operation is a method here).
pub struct AsCore {
    root: u64,
    inner: Mutex<Inner>,
    me: Weak<AsCore>,
    /// The kept page and table numbers (`counters.rs`; changed under `inner`, read without it).
    ctr: Counters,
    /// MM-tlb2 fixer: whether the last fault on a missing page ended as an anonymous fill; only then does the
    /// next one zero a frame before taking `inner` (a run of file or device faults zeroes nothing in vain).
    anon_hint: core::sync::atomic::AtomicBool,
}

/// The 8 KiB block of a user root: the PML4 and its TLB context (`hal::paging::new_user_root`).
fn alloc_root_block() -> Option<u64> {
    mm_core::alloc_zeroed_block(1, mm_core::Zone::Normal, mm_core::Kind::PageTable)
}

pub(crate) fn page_flags(p: Prot) -> PageFlags {
    // PROT_NONE keeps a page that has a frame present for the kernel only: user access faults, the content
    // stays for a later mprotect.
    if p.accessible() { PageFlags::user(p.write, p.exec) } else { PageFlags(PageFlags::NX) }
}

/// The rights of a frame mapped read-only on purpose (MAP_PRIVATE before its copy, a page shared by fork, a
/// clean page of a file or device that tracks dirty pages).
pub(crate) fn read_only(p: Prot) -> PageFlags {
    if p.accessible() { PageFlags::user(false, p.exec) } else { PageFlags(PageFlags::NX) }
}

/// Must a read-only present page of VMA `v` stay read-only when the VMA becomes writable? Yes for page-cache
/// frames of private mappings and frames another space still shares (their first write copies), and for pages
/// whose writes a file or device tracks (their first write marks them dirty). A program's own frame that only
/// this space maps can simply become writable (Linux `can_change_pte_writable`).
pub(crate) fn keep_read_only(v: &Vma, phys: u64) -> bool {
    match &v.file {
        Some(f) if f.device.is_some() => true,
        Some(f) if f.shared => f.backing.track_dirty(),
        _ => !(mm_core::is_user_frame(phys) && share::exclusive(phys)),
    }
}

fn to_mm(e: Errno) -> MmError {
    match e {
        Errno::NoMem => MmError::NoMemory,
        Errno::Exist => MmError::Exists,
        _ => MmError::Invalid,
    }
}

fn linux_prot(p: Prot) -> u64 {
    p.read as u64 | (p.write as u64) << 1 | (p.exec as u64) << 2
}

pub(crate) fn free_user(phys: u64) {
    if let Some(fa) = mm_core::frames() {
        fa.free(phys, 0, mm_core::Kind::User);
    }
}

/// This space no longer maps the program frame `phys`: freed when no other space shares it.
pub(crate) fn release_user(phys: u64) {
    if share::release(phys) {
        free_user(phys);
    }
}

impl AddressSpace {
    /// A new, empty address space that fills pages at map time (`Fill::Eager`).
    pub fn new() -> Result<Self, MmError> {
        Self::with_fill(Fill::Eager)
    }

    pub fn with_fill(fill: Fill) -> Result<Self, MmError> {
        let root = paging::new_user_root(crate::kernel_root(), &mut alloc_root_block).ok_or(MmError::NoMemory)?;
        Ok(Self::from_inner(root, Inner::new(fill), Counters::default()))
    }

    /// `brk(new)` (also here so it wins over trait methods of the same name on `AddressSpace`).
    pub fn brk(&self, new: u64) -> u64 {
        self.core.sys_brk(new)
    }

    fn from_inner(root: u64, inner: Inner, ctr: Counters) -> Self {
        let core = Arc::new_cyclic(|me| AsCore { root, inner: Mutex::named("mm.as", inner), me: me.clone(), ctr, anon_hint: core::sync::atomic::AtomicBool::new(true) });
        AddressSpace { core }
    }
}

impl AsCore {
    /// Physical address of the PML4.
    pub fn root(&self) -> u64 {
        self.root
    }

    pub fn fill(&self) -> Fill {
        self.inner.lock().fill
    }

    /// Loads this root on the running CPU (it joins the root's CPU mask, `hal::paging::tlb`).
    pub fn activate(&self) {
        paging::activate(self.root);
    }

    /// The CPUs that have this root loaded now (lazily too): the targets of its shootdowns.
    pub fn loaded_on(&self) -> Vec<usize> {
        let mut v = Vec::new();
        paging::tlb::cpus_with(self.root, &mut |c| {
            if v.try_reserve(1).is_ok() {
                v.push(c);
            }
        });
        v
    }

    pub fn vmas(&self) -> Vec<Vma> {
        self.inner.lock().space.tree.to_vec()
    }

    /// The current program break.
    pub fn current_brk(&self) -> u64 {
        self.inner.lock().space.brk
    }

    /// Pages that have a frame now (present leaves of the user half, device pages included; kept, no walk).
    pub fn resident_pages(&self) -> u64 {
        self.ctr.present()
    }

    /// ktests that map a program frame with `hal::paging` directly: count it as MM's own fill would.
    pub(crate) fn test_count_anon(&self) {
        self.ctr.add(Class::Anon, 1);
    }

    /// A page-table frame for one of this space's entries (counted for VmPTE).
    pub(crate) fn alloc_table(&self) -> Option<u64> {
        let t = mm_core::alloc_table_frame();
        if t.is_some() {
            self.ctr.tables_added(1);
        }
        t
    }

    /// Pages this space holds of the system's commit (its VM_ACCOUNT mappings).
    pub fn committed_pages(&self) -> u64 {
        self.inner.lock().space.committed
    }

    /// This space's own commit limit in pages (None: only the system's). A space already above it keeps what it
    /// has; only new charges are refused.
    pub fn set_commit_limit(&self, pages: Option<u64>) {
        self.inner.lock().limit = pages;
    }

    pub fn commit_limit(&self) -> Option<u64> {
        self.inner.lock().limit
    }

    /// ktests only: the next forks of this space fail (ENOMEM) after mapping `pages` pages in the child, so the
    /// clean-up of a fork that fails half-way can be checked. None switches it off.
    #[doc(hidden)]
    pub fn test_fail_fork_after(&self, pages: Option<u64>) {
        self.inner.lock().fork_fail_after = pages;
    }

    /// Frees an unmapped frame or emptied table whose shootdown was confirmed (a file page goes back to its
    /// backing, a device's frame stays the device's), or queues it for the drop. Never allocates by the size of
    /// the range: only the queue of the unconfirmed case grows (one entry per frame), and a full kernel heap
    /// there counts the frame as lost instead of aborting.
    fn give_back(r: &Released, owner: &Owner, deferred: &mut Vec<Deferred>, lost: &mut u64) {
        if r.confirmed {
            if r.table {
                if let Some(fa) = mm_core::frames() {
                    fa.free(r.phys, 0, mm_core::Kind::PageTable);
                }
                return;
            }
            match owner {
                Owner::Device => {}
                Owner::File(b, _) => {
                    if !b.put_page(r.phys) {
                        release_user(r.phys);
                    }
                }
                Owner::Anon => release_user(r.phys),
            }
            return;
        }
        if !r.table && matches!(owner, Owner::Device) {
            return;
        }
        if deferred.try_reserve(1).is_ok() {
            DEFERRED_FRAMES.inc();
            deferred.push(match (r.table, owner) {
                (true, _) => Deferred::Table(r.phys),
                (false, Owner::File(b, _)) => Deferred::FilePage(r.phys, b.clone()),
                (false, _) => Deferred::Frame(r.phys),
            });
        } else {
            FRAMES_LOST.inc();
            *lost += 1;
        }
    }

    /// The file and device VMAs overlapping [start, end), clipped to it (taken before the tree changes).
    pub(crate) fn segments(g: &Inner, start: u64, end: u64) -> Result<Segs, Errno> {
        let mut v: Segs = Vec::new();
        for x in g.space.tree.overlapping(start, end) {
            if let Some(f) = &x.file {
                v.try_reserve(1).map_err(|_| Errno::NoMem)?;
                let owner = if f.device.is_some() { Owner::Device } else { Owner::File(f.backing.clone(), x.kind == VmaKind::Anonymous) };
                v.push((x.start.max(start), x.end.min(end), owner));
            }
        }
        Ok(v)
    }

    /// Removes the pages of [start, end) and gives their frames and emptied tables back once the shootdown that
    /// covers them is confirmed (`unmap_user`: rounds of up to 512 frames, tables after the last). The file and
    /// device segments go first, each with its owner; what is left is the program's own memory.
    pub(crate) fn unmap_pages(&self, g: &mut Inner, start: u64, end: u64, segs: &Segs) {
        for (s, e, o) in segs {
            self.unmap_one(g, *s, *e, o);
        }
        self.unmap_one(g, start, end, &Owner::Anon);
    }

    fn unmap_one(&self, g: &mut Inner, start: u64, end: u64, owner: &Owner) {
        let lost_before = g.lost;
        let (d, l, ctr) = (&mut g.deferred, &mut g.lost, &self.ctr);
        // The counters go down as each entry goes (classified before `give_back` may free the frame), also for
        // the frames of an unconfirmed round that wait in `deferred`: the space no longer maps them.
        let r = paging::unmap_user(self.root, start, end - start, &mut |r: Released| {
            if r.table {
                ctr.tables_freed(1);
            } else {
                let c = Class::of(r.phys, owner);
                ctr.sub(c, 1);
                if c.cached() {
                    share::unmap_file(r.phys);
                }
            }
            Self::give_back(&r, owner, d, l)
        });
        match r {
            Err(e) => klog::event!(Error, "mm", "as.unmap_failed", start = klog::Hex(start), end = klog::Hex(end), error = alloc::format!("{e:?}")),
            Ok(u) if !u.outcome.confirmed() => {
                UNCONFIRMED.inc();
                klog::event!(Warn, "mm", "as.shootdown_unconfirmed", root = klog::Hex(self.root), start = klog::Hex(start), end = klog::Hex(end),
                    cpus = u.outcome.unanswered, resends = u.outcome.resends, not_ready = u.outcome.not_ready;
                    "a CPU did not answer the TLB shootdown: the unmapped frames stay out of use until the address space ends");
            }
            Ok(_) => {}
        }
        if g.lost > lost_before {
            klog::event!(Warn, "mm", "as.frames_lost", root = klog::Hex(self.root), frames = g.lost - lost_before;
                "no room to queue unmapped frames another CPU may cache: kept out of use");
        }
    }

    /// Removes [start, end) from the tree and its pages, giving the commit back (munmap, MAP_FIXED, mremap).
    pub(crate) fn remove_range(&self, g: &mut Inner, start: u64, end: u64) -> Result<(), Errno> {
        let segs = Self::segments(g, start, end)?;
        let before = g.space.committed;
        if g.space.commit_munmap(start, end)? {
            self.unmap_pages(g, start, end, &segs);
            self.rmap_forget(g, &segs);
        }
        g.settle(before, 0);
        Ok(())
    }

    pub(crate) fn mmap_locked(&self, g: &mut Inner, plan: &MmapPlan, kind: VmaKind, file: Option<FileRef>) -> Result<u64, Errno> {
        let end = plan.addr + plan.len;
        g.space.tree.reserve(3).map_err(|_| Errno::NoMem)?;
        // Linux's order (mm/mmap.c:2693-2702): what MAP_FIXED replaces goes first, then the charge; a refused
        // charge leaves the old mapping gone.
        if plan.replaces {
            self.remove_range(g, plan.addr, end)?;
        }
        // MM-rmap: the file's reverse-map entry first (a refusal changes nothing more than MAP_FIXED did).
        let key = file.as_ref().and_then(rmap::key_of);
        if let Some(k) = key
            && !rmap::add(k, &self.me)
        {
            return Err(Errno::NoMem);
        }
        let need = if plan.account { plan.len / PAGE } else { 0 };
        if let Err(e) = g.charge(need, 0) {
            if let Some(k) = key {
                self.rmap_forget_key(g, k);
            }
            return Err(e);
        }
        let before = g.space.committed;
        let p = MmapPlan { replaces: false, ..*plan };
        if let Err(e) = g.space.commit_mmap_with(&p, kind, file) {
            commit::uncharge(need);
            if let Some(k) = key {
                self.rmap_forget_key(g, k);
            }
            return Err(e);
        }
        g.settle(before, need);
        // A failed MAP_POPULATE alone is ignored, as on Linux; an eager space must not keep empty pages.
        if (g.fill == Fill::Eager || plan.populate)
            && let Err(e) = self.populate(g, plan.addr, end, true)
            && g.fill == Fill::Eager
        {
            let _ = self.remove_range(g, plan.addr, end);
            klog::event!(Warn, "mm", "as.populate_failed", addr = klog::Hex(plan.addr), len = plan.len, reason = e.text(), replaced = plan.replaces;
                "mmap refused with ENOMEM; with replaced=true the old mapping there is gone too (as on Linux)");
            return Err(Errno::NoMem);
        }
        Ok(plan.addr)
    }

    /// `mmap` of anonymous memory with Linux's semantics (MAP_SHARED: `shm.rs`; file mappings: `sys_mmap_file`).
    pub fn sys_mmap(&self, addr: u64, len: u64, prot: u64, flags: u64, off: u64) -> Result<u64, Errno> {
        if flags & MAP_ANONYMOUS != 0 && flags & crate::linux::MAP_TYPE == crate::linux::MAP_SHARED {
            return self.mmap_shared_anon(addr, len, prot, flags, off);
        }
        let mut g = self.inner.lock();
        let plan = g.space.plan_mmap(addr, len, prot, flags, off)?;
        self.mmap_locked(&mut g, &plan, VmaKind::Anonymous, None)
    }

    /// FS-5: `mmap` of a file. The syscall layer checked the offset alignment, the fd (EBADF) and MAP_HUGETLB
    /// (EINVAL) first, as Linux's `ksys_mmap_pgoff`; here come the address checks (`do_mmap` up to EEXIST),
    /// then `check(len)` with the file's own checks (`vfs::mmap::check`), then the mapping. Errors are errnos.
    /// A private writable file mapping is charged like anonymous memory (its pages become private copies).
    pub fn sys_mmap_file(&self, addr: u64, len: u64, prot: u64, flags: u64, off: u64, check: &mut dyn FnMut(u64) -> Result<Mapped, i64>) -> Result<u64, i64> {
        let mut g = self.inner.lock();
        let mut plan = g.space.plan_mmap_file(addr, len, prot, flags, off).map_err(|e| e.code())?;
        let m = check(plan.len)?;
        plan.account = !m.backing.may_share() && plan.prot.write;
        let file = FileRef::file(m.backing, off >> 12, m.shared, m.may_write).exec(m.may_exec);
        self.mmap_locked(&mut g, &plan, VmaKind::File, Some(file)).map_err(|e| e.code())
    }

    pub fn sys_munmap(&self, addr: u64, len: u64) -> Result<(), Errno> {
        let mut g = self.inner.lock();
        let (start, end) = g.space.plan_munmap(addr, len)?;
        self.remove_range(&mut g, start, end)
    }

    pub fn sys_mprotect(&self, addr: u64, len: u64, prot: u64) -> Result<(), Errno> {
        let mut g = self.inner.lock();
        let Some(mut p) = g.space.plan_mprotect(addr, len, prot)? else { return Ok(()) };
        if p.applied_end <= p.start {
            return p.error.map_or(Ok(()), Err);
        }
        g.space.tree.reserve(2).map_err(|_| Errno::NoMem)?;
        // The commit, VMA by VMA in address order (mm/mprotect.c:633): a refusal stops there with ENOMEM after
        // the VMAs before it changed, as Linux's loop does.
        let mut charged = 0;
        for v in g.space.tree.overlapping(p.start, p.applied_end) {
            if Space::protect_charges(v, p.prot) {
                let pages = (v.end.min(p.applied_end) - v.start.max(p.start)) / PAGE;
                if g.charge(pages, charged).is_err() {
                    p.applied_end = v.start.max(p.start);
                    p.error = Some(Errno::NoMem);
                    break;
                }
                charged += pages;
            }
        }
        if p.applied_end > p.start {
            if g.fill == Fill::Eager && p.prot.accessible() && self.populate(&g, p.start, p.applied_end, false).is_err() {
                commit::uncharge(charged);
                return Err(Errno::NoMem);
            }
            let before = g.space.committed;
            if let Err(e) = g.space.commit_mprotect(&p) {
                commit::uncharge(charged);
                return Err(e);
            }
            g.settle(before, charged);
            let (fl, ro, tree) = (page_flags(p.prot), read_only(p.prot), &g.space.tree);
            let mut leaf = |u: hal::paging::Unmapped| {
                let keep = p.prot.write && !u.flags.contains(PageFlags::WRITABLE) && tree.find(u.virt).is_some_and(|v| keep_read_only(v, u.phys));
                // The accessed and dirty bits stay (smaps' Referenced and Dirty, as Linux's mprotect keeps them).
                Some(PageFlags((if keep { ro } else { fl }).0 | (u.flags.0 & (CACHE_BITS | PageFlags::ACCESSED | PageFlags::DIRTY))))
            };
            match paging::protect_user(self.root, p.start, p.applied_end - p.start, &mut leaf) {
                Err(e) => klog::event!(Error, "mm", "as.protect_failed", start = klog::Hex(p.start), error = alloc::format!("{e:?}")),
                Ok((_, o)) if !o.confirmed() => {
                    UNCONFIRMED.inc();
                    klog::event!(Warn, "mm", "as.shootdown_unconfirmed", root = klog::Hex(self.root), start = klog::Hex(p.start),
                        end = klog::Hex(p.applied_end), cpus = o.unanswered, resends = o.resends, not_ready = o.not_ready;
                        "a CPU did not answer the TLB shootdown of an mprotect: its pending IPI flushes before it runs user code again");
                }
                Ok(_) => {}
            }
        }
        p.error.map_or(Ok(()), Err)
    }

    /// msync(start, len, flags) as linux-6.6/mm/msync.c:32-108: EINVAL for unknown flags, an unaligned start or
    /// ASYNC|SYNC; the length page-aligned with wrap-around (a length within a page of 2^64 is 0: answer 0);
    /// ENOMEM for a range past 2^64; holes are skipped but answer ENOMEM at the end (at once for
    /// a plain MS_ASYNC); MS_SYNC writes the range of every shared file VMA back (`MapBacking::sync`), after
    /// the space's lock is released. There is no mlock, so MS_INVALIDATE never meets VM_LOCKED (EBUSY).
    pub fn sys_msync(&self, start: u64, len: u64, flags: u64) -> Result<(), i64> {
        use uapi::errno::{EINVAL, ENOMEM};
        if flags & !(MS_ASYNC | MS_INVALIDATE | MS_SYNC) != 0 || start & (PAGE - 1) != 0 || (flags & MS_ASYNC != 0 && flags & MS_SYNC != 0) {
            return Err(EINVAL);
        }
        // PAGE_ALIGN wraps as on Linux: a length within a page of 2^64 becomes 0 and the call answers 0.
        let len = len.wrapping_add(PAGE - 1) & !(PAGE - 1);
        let end = start.wrapping_add(len);
        if end < start {
            return Err(ENOMEM);
        }
        if end == start {
            return Ok(());
        }
        let mut jobs: Vec<(Arc<dyn MapBacking>, u64, u64)> = Vec::new();
        let mut unmapped_error = 0;
        let mut error;
        {
            let g = self.inner.lock();
            let mut at = start;
            let mut vma = g.space.tree.next_from(at);
            loop {
                error = ENOMEM;
                let Some(v) = vma else { break };
                if at < v.start {
                    if flags == MS_ASYNC {
                        break;
                    }
                    at = v.start;
                    if at >= end {
                        break;
                    }
                    unmapped_error = ENOMEM;
                }
                let fstart = v.pgoff_at(at);
                let fend = v.pgoff_at(end.min(v.end) - 1);
                at = v.end;
                if flags & MS_SYNC != 0
                    && let (Some(f), Some(a), Some(b)) = (&v.file, fstart, fend)
                    && f.shared
                {
                    jobs.try_reserve(1).map_err(|_| ENOMEM)?;
                    jobs.push((f.backing.clone(), a, b));
                }
                if at >= end {
                    error = 0;
                    break;
                }
                vma = g.space.tree.next_from(at);
            }
        }
        for (b, a, z) in jobs {
            b.sync(a, z)?;
        }
        if error != 0 {
            return Err(error);
        }
        if unmapped_error != 0 { Err(unmapped_error) } else { Ok(()) }
    }

    /// `brk(new)`: the new break, or the old one when it cannot move (Linux semantics; growth is charged to the
    /// commit like `do_brk_flags`, a refusal keeps the old break).
    pub fn sys_brk(&self, new: u64) -> u64 {
        let mut g = self.inner.lock();
        let old = g.space.brk;
        let plan = g.space.plan_brk(new);
        let need = match plan {
            BrkPlan::Move { map: Some((s, e)), .. } => (e - s) / PAGE,
            _ => 0,
        };
        if g.charge(need, 0).is_err() {
            return old;
        }
        let before = g.space.committed;
        let Ok(b) = g.space.commit_brk(&plan) else {
            commit::uncharge(need);
            return old;
        };
        g.settle(before, need);
        if let BrkPlan::Move { unmap, map, .. } = plan {
            if let Some((s, e)) = unmap {
                self.unmap_pages(&mut g, s, e, &Vec::new());
            }
            if let Some((s, e)) = map
                && g.fill == Fill::Eager
                && self.populate(&g, s, e, true).is_err()
            {
                let _ = self.remove_range(&mut g, s, e);
                g.space.brk = old;
                klog::event!(Warn, "mm", "as.brk_populate_failed", from = klog::Hex(s), to = klog::Hex(e));
                return old;
            }
        }
        b
    }

    /// Maps anonymous zeroed memory at exactly [start, start+len) (both page-aligned, nothing mapped there);
    /// writable memory is charged to the commit.
    pub fn map_fixed(&self, start: u64, len: u64, prot: Prot, kind: VmaKind) -> Result<(), MmError> {
        if !start.is_multiple_of(PAGE) || len == 0 || !len.is_multiple_of(PAGE) || start < PAGE {
            return Err(MmError::Invalid);
        }
        let end = start.checked_add(len).filter(|&e| e <= hal::USER_TOP).ok_or(MmError::Invalid)?;
        let mut g = self.inner.lock();
        if !g.space.tree.is_free(start, end) {
            return Err(MmError::Exists);
        }
        let plan = MmapPlan { addr: start, len, prot, replaces: false, populate: false, shared: false, account: prot.write };
        self.mmap_locked(&mut g, &plan, kind, None).map(|_| ()).map_err(to_mm)
    }

    /// `mmap(0, len, prot, MAP_PRIVATE|MAP_ANONYMOUS)`: returns the chosen address.
    pub fn map_anywhere(&self, len: u64, prot: Prot) -> Result<u64, MmError> {
        self.sys_mmap(0, len, linux_prot(prot), MAP_PRIVATE | MAP_ANONYMOUS, 0).map_err(to_mm)
    }

    pub fn unmap(&self, start: u64, len: u64) -> Result<(), MmError> {
        self.sys_munmap(start, len).map_err(to_mm)
    }

    pub fn protect(&self, start: u64, len: u64, prot: Prot) -> Result<(), MmError> {
        self.sys_mprotect(start, len, linux_prot(prot)).map_err(to_mm)
    }

    /// Sets where the heap starts (after the ELF image).
    pub fn init_brk(&self, start: u64) {
        self.inner.lock().space.init_brk(start);
    }

    /// `brk(new)`: returns the current break (Linux semantics: on failure the old value).
    pub fn brk(&self, new: u64) -> u64 {
        self.sys_brk(new)
    }
}

/// Why a dying address space must be kept instead of freed (None: free it). `still_loaded`: this CPU could
/// not leave the root (no kernel root to load); `confirmed`: every other CPU that had it loaded left it.
pub(crate) fn drop_verdict(still_loaded: bool, confirmed: bool) -> Option<&'static str> {
    if still_loaded {
        Some("loaded_root_no_kernel_root")
    } else if !confirmed {
        Some("shootdown_unconfirmed")
    } else {
        None
    }
}

impl Drop for AsCore {
    fn drop(&mut self) {
        let root = self.root;
        // Every CPU that still has the root loaded (this one, or one that keeps it lazily) switches to the kernel
        // root first (MM-6); freeing the tables before that could hand out frames a CPU still walks.
        let o = paging::tlb::leave(root);
        Self::rmap_drop_of(self as *const AsCore);
        let g = self.inner.get_mut();
        // The program is gone: its charge goes back whatever happens to the frames.
        commit::uncharge(g.space.committed);
        if let Some(why) = drop_verdict(paging::current_root() == root, o.confirmed()) {
            AS_LEAKED.inc();
            klog::event!(Warn, "mm", "as.drop_leaked", root = klog::Hex(root), reason = why, vmas = g.space.tree.len(), cpus = o.unanswered;
                "address space tables and frames kept out of use instead of freed");
            // The file pages stay pinned by their mappings (whose `Drop` keeps pinned frames out of use).
            return;
        }
        let fa = mm_core::frames();
        let tree = &g.space.tree;
        // What the teardown finds, by class and tables, against the kept counters (Linux `check_mm`).
        let (mut found, mut tables) = ([0u64; 4], 0u64);
        paging::teardown_user_half(
            root,
            &mut |va, pa, _fl| match {
                let v = tree.find(va);
                found[Class::in_vma(pa, v) as usize] += 1;
                v.and_then(|v| v.file.as_ref())
            } {
                Some(f) if f.device.is_some() => {}
                // A file VMA's page goes back to its backing (its map count first: the backing may free it);
                // anything else (incl. private copies) is the program's.
                Some(f) => {
                    if !mm_core::is_user_frame(pa) {
                        share::unmap_file(pa);
                    }
                    if !f.backing.put_page(pa) {
                        release_user(pa);
                    }
                }
                _ => release_user(pa),
            },
            &mut |t| {
                tables += 1;
                if let Some(fa) = fa {
                    fa.free(t, 0, mm_core::Kind::PageTable);
                }
            },
        );
        self.ctr.check_teardown(root, &found, tables);
        paging::retire_user_root(root);
        if let Some(fa) = fa {
            for d in g.deferred.drain(..) {
                match d {
                    Deferred::Frame(p) => release_user(p),
                    Deferred::Table(p) => fa.free(p, 0, mm_core::Kind::PageTable),
                    Deferred::FilePage(p, b) => {
                        if !b.put_page(p) {
                            release_user(p);
                        }
                    }
                }
            }
            fa.free(root, 1, mm_core::Kind::PageTable);
        }
        if g.lost > 0 {
            klog::event!(Warn, "mm", "as.frames_lost_total", root = klog::Hex(root), frames = g.lost; "unmapped frames that could not be queued for freeing");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::drop_verdict;

    #[test]
    fn drop_never_frees_a_root_still_in_use() {
        assert_eq!(drop_verdict(false, true), None);
        assert_eq!(drop_verdict(true, true), Some("loaded_root_no_kernel_root"));
        assert_eq!(drop_verdict(false, false), Some("shootdown_unconfirmed"), "another CPU may still walk it");
        assert_eq!(drop_verdict(true, false), Some("loaded_root_no_kernel_root"));
    }
}
