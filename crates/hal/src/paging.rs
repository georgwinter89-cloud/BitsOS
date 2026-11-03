//! Page-table primitives (owner: MM). 4-level paging; tables are reached through the direct map.
//!
//! Soundness rule for callers (mm, mmio): a physical frame mapped writable must not also be a live Rust object
//! of the kernel. MM guarantees it by mapping only frames it allocated for that purpose.
//!
//! MM-2: the walk (`walk`) handles 4 KiB, 2 MiB and 1 GiB leaves and unmapping that frees empty tables; the
//! kernel builds its own root (`kernel`; the plan with the direct map typed by the MTRRs, the W^X kernel image
//! and the vmap allocator lives in `mm`) and switches to it; `probe` causes deliberate faults for the W^X and
//! guard-page ktests. MM-5a adds the user-range operations (`unmap_user`, `protect_user`) and the kernel-VA
//! hook (`kva`). MM-6 (`tlb`): every CR3 load goes through `activate`/`tlb`, user roots carry a context with
//! their CPU mask and generation, PCID per CPU, lazy TLB, and every unmap or protection change here shoots
//! the old translations down on the other CPUs before frames or tables are handed back.

mod kernel;
mod kva;
pub mod probe;
mod retype;
pub mod tlb;
mod tlb_cause;
mod tlb_plan;
mod walk;
/// Frames one unmap round gathers at most (Linux's mmu_gather bound; tests derive round counts from it).
pub use tlb_plan::GATHER_MAX;

pub use kernel::{
    adopt_leaves, device_mapper, flush_all, free_unused_root, kernel_image, kernel_root, map_device_window, map_kernel,
    new_kernel_root, set_device_mapper, set_table_frames, switch_kernel_root, table_frames_ready, unmap_kernel, DeviceMapper,
    DeviceWindow, SwitchError, VmapError, KERNEL_HALF,
};
pub use kva::{kernel_va_ready, release_kernel_va, reserve_kernel_va, set_kernel_va, KernelVaRelease, KernelVaReserve};
pub use retype::{set_direct_map_cache, RetypeStats};
pub use tlb::{activate_lazy_kernel, home_root, thread_root, Outcome};
pub use walk::{Leaf, PageSize, UnmapStats, Unmapped, PAT_LARGE};

use crate::phys_to_virt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFlags(pub u64);

impl PageFlags {
    pub const PRESENT: u64 = 1;
    pub const WRITABLE: u64 = 1 << 1;
    pub const USER: u64 = 1 << 2;
    pub const PWT: u64 = 1 << 3;
    pub const PCD: u64 = 1 << 4;
    pub const ACCESSED: u64 = 1 << 5;
    pub const DIRTY: u64 = 1 << 6;
    pub const HUGE: u64 = 1 << 7;
    /// PAT bit of a 4 KiB leaf (the bit HUGE uses in upper levels; `walk` moves it to bit 12 in large leaves).
    pub const PAT_4K: u64 = 1 << 7;
    pub const GLOBAL: u64 = 1 << 8;
    pub const NX: u64 = 1 << 63;

    pub fn kernel_data() -> Self {
        PageFlags(Self::PRESENT | Self::WRITABLE | Self::NX)
    }
    pub fn user(writable: bool, executable: bool) -> Self {
        let mut f = Self::PRESENT | Self::USER;
        if writable {
            f |= Self::WRITABLE;
        }
        if !executable {
            f |= Self::NX;
        }
        PageFlags(f)
    }
    pub fn with_cache(self, c: CacheType) -> Self {
        PageFlags((self.0 & !(Self::PWT | Self::PCD | Self::PAT_4K)) | c.bits())
    }
    pub fn contains(&self, bits: u64) -> bool {
        self.0 & bits == bits
    }
    /// The memory type these leaf flags (4 KiB layout) select through the PAT layout of `pat.rs`.
    pub fn cache(&self) -> Option<CacheType> {
        CacheType::from_bits(self.0)
    }
}

/// Memory types through the PAT layout of `pat.rs` (entries 0-3; the PAT bit stays 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CacheType {
    WriteBack,
    WriteCombining,
    UncachedMinus,
    Uncached,
}

impl CacheType {
    pub fn bits(self) -> u64 {
        match self {
            CacheType::WriteBack => 0,
            CacheType::WriteCombining => PageFlags::PWT,
            CacheType::UncachedMinus => PageFlags::PCD,
            CacheType::Uncached => PageFlags::PCD | PageFlags::PWT,
        }
    }
    /// The PAT entry (0-7) a leaf with these bits (4 KiB layout) selects.
    pub fn pat_index(bits: u64) -> usize {
        (((bits >> 3) & 1) | ((bits >> 4) & 1) << 1 | ((bits >> 7) & 1) << 2) as usize
    }
    /// The type of entries 0-3; `None` for the upper four (BitsOS never selects them).
    pub fn from_bits(bits: u64) -> Option<CacheType> {
        match Self::pat_index(bits) {
            0 => Some(CacheType::WriteBack),
            1 => Some(CacheType::WriteCombining),
            2 => Some(CacheType::UncachedMinus),
            3 => Some(CacheType::Uncached),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// No frame for an intermediate table.
    NoMemory,
    /// The address is already mapped (never replaces a mapping silently).
    AlreadyMapped,
    /// A large page covers the address.
    HugePageInTheWay,
    NotAligned,
    /// An unmap range covers only part of a large page.
    PartialHugePage,
    /// Outside the range the call allows (kernel mappings: the kernel half), or no table frame source yet.
    WrongRange,
    /// The CPU lacks the feature (a 1 GiB leaf without PDPE1GB).
    Unsupported,
    /// Another CPU did not confirm the TLB shootdown in time (or the shootdown vector is not set up yet):
    /// the change is done, but what it freed must stay out of use (MM-6).
    NeedsShootdown,
    /// A page of the range is not mapped (retyping the direct map: not RAM the kernel maps).
    NotMapped,
}

pub(crate) const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

fn table(phys: u64) -> *mut u64 {
    phys_to_virt(phys) as *mut u64
}

fn read_entry(t: u64, i: usize) -> u64 {
    // Atomic, because lockless walkers (futex words in ksync's user_word, the panic backtrace) read live user
    // entries while `split_user_2m` swaps them; a relaxed load is a plain aligned `mov` on x86.
    // SAFETY: `t` is a page-table frame (RAM, reachable through the direct map, 4 KiB aligned, so every entry is
    // an aligned u64); i < 512 (walk indexes). Entries are only ever accessed as whole u64 words.
    unsafe { core::sync::atomic::AtomicU64::from_ptr(table(t).add(i)) }.load(core::sync::atomic::Ordering::Relaxed)
}

/// Page tables reached through the direct map; table frames from the given closures.
struct Direct<'a> {
    alloc: &'a mut dyn FnMut() -> Option<u64>,
    free: &'a mut dyn FnMut(u64),
}

impl walk::Tables for Direct<'_> {
    fn read(&self, t: u64, i: usize) -> u64 {
        read_entry(t, i)
    }
    fn write(&mut self, t: u64, i: usize, v: u64) {
        // SAFETY: as in `read_entry`; page-table frames are never Rust objects (MM's soundness rule above).
        unsafe { table(t).add(i).write_volatile(v) }
    }
    fn alloc_table(&mut self) -> Option<u64> {
        let f = (self.alloc)()?;
        // SAFETY: a fresh frame from the allocator, direct-mapped, 4 KiB, not yet known to anyone.
        unsafe { core::ptr::write_bytes(phys_to_virt(f) as *mut u8, 0, 4096) };
        Some(f)
    }
    fn free_table(&mut self, t: u64) {
        (self.free)(t)
    }
}

/// Read-only walks: never allocates, frees or writes.
struct Reader;

impl walk::Tables for Reader {
    fn read(&self, t: u64, i: usize) -> u64 {
        read_entry(t, i)
    }
    fn write(&mut self, _t: u64, _i: usize, _v: u64) {}
    fn alloc_table(&mut self) -> Option<u64> {
        None
    }
    fn free_table(&mut self, _t: u64) {}
}

/// The physical address of the active PML4.
pub fn current_root() -> u64 {
    crate::cpu::read_cr3() & ADDR_MASK
}

/// Loads `root` into CR3 on this CPU through `tlb` (PCID, CPU mask, generation check; a user root already
/// loaded here stays without a flush). Callers pass a root whose kernel half maps the running code, stack, IDT
/// and GDT to the same frames with the rights they need: a user root that copies the kernel root's half
/// (`new_user_root`), the kernel root itself, or the new kernel root after `switch_kernel_root` checked exactly
/// that. Low bits (a PCID saved with a CR3 value) are ignored. Any other root (a user root whose space has
/// ended: `current_root()` read while this CPU held it lazily, then kept) is never loaded: the kernel root is,
/// counted in `tlb::stats().foreign_refused`. For "the kernel root" use `home_root()`, not `current_root()`.
pub fn activate(root: u64) {
    tlb::switch_root(root & ADDR_MASK)
}

/// `activate` for a user root a thread saved at its last switch: loads it only while its address space lives
/// (a context that is neither retired nor dying), else the kernel root (hal::switch).
pub fn activate_saved(root: u64) {
    tlb::switch_saved_root(root & ADDR_MASK)
}

pub fn flush_page(virt: u64) {
    #[cfg(target_os = "none")]
    // SAFETY: invlpg only drops TLB entries (global ones too) and paging-structure caches.
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags))
    };
    #[cfg(not(target_os = "none"))]
    let _ = virt;
}

/// Maps one 4 KiB page. `alloc` returns a free frame for intermediate tables (zeroed here). Kernel-half
/// pages are always global: every root shares the kernel half, and the shootdown relies on `invlpg` dropping
/// them in every PCID (`tlb`).
pub fn map_4k(root: u64, virt: u64, phys: u64, flags: PageFlags, alloc: &mut dyn FnMut() -> Option<u64>) -> Result<(), MapError> {
    let mut free = |_| {};
    let f = if virt >= KERNEL_HALF { flags.0 | PageFlags::GLOBAL } else { flags.0 };
    walk::map(&mut Direct { alloc, free: &mut free }, root, virt, phys, PageSize::Size4K, f)
}

/// Maps one 4 KiB page in the user half of `root` (below `USER_TOP`); every table entry on the way carries
/// USER and WRITABLE, so the leaf alone decides (it may be kernel-only for a PROT_NONE page with content).
pub fn map_user_4k(root: u64, virt: u64, phys: u64, flags: PageFlags, alloc: &mut dyn FnMut() -> Option<u64>) -> Result<(), MapError> {
    if virt >= crate::USER_TOP {
        return Err(MapError::WrongRange);
    }
    let mut free = |_| {};
    walk::map_in(&mut Direct { alloc, free: &mut free }, root, virt, phys, PageSize::Size4K, user_bits(flags), true)
}

/// MM-13: maps one 2 MiB page (a transparent huge page) at `virt` -> `phys` (both 2 MiB aligned) in `root`'s
/// user half. The page-directory slot must be empty: a page table there (even an empty one) is `AlreadyMapped`.
pub fn map_user_2m(root: u64, virt: u64, phys: u64, flags: PageFlags, alloc: &mut dyn FnMut() -> Option<u64>) -> Result<(), MapError> {
    if virt.checked_add(PageSize::Size2M.bytes()).is_none_or(|e| e > crate::USER_TOP) {
        return Err(MapError::WrongRange);
    }
    let mut free = |_| {};
    walk::map_in(&mut Direct { alloc, free: &mut free }, root, virt, phys, PageSize::Size2M, user_bits(flags), true)
}

/// MM-13: could a 2 MiB page be mapped at `virt` (2 MiB aligned) of `root` without touching anything: no page
/// table and no leaf in its page-directory slot (Linux `pmd_none`)?
pub fn huge_slot_free(root: u64, virt: u64) -> bool {
    if virt >= crate::USER_TOP || !virt.is_multiple_of(PageSize::Size2M.bytes()) {
        return false;
    }
    let mut t = root;
    for level in [4, 3] {
        let e = read_entry(t, walk::index(virt, level));
        if e & PageFlags::PRESENT == 0 {
            return true;
        }
        if e & PageFlags::HUGE != 0 {
            return false;
        }
        t = e & ADDR_MASK;
    }
    read_entry(t, walk::index(virt, 2)) & PageFlags::PRESENT == 0
}

/// MM-13: splits the 2 MiB user leaf at `virt` (2 MiB aligned) of `root` into 512 4 KiB entries in the page
/// table `pt` (a frame the caller owns and hands over on success; its old content does not matter). Same frames,
/// rights, memory type and accessed/dirty bits. Linux's order (`__split_huge_pmd_locked`,
/// linux-6.6/mm/huge_memory.c:2066, `pmdp_invalidate`): the 2 MiB entry is taken out and shot down first, so no
/// CPU ever holds a 2 MiB and a 4 KiB translation of one address (the CPUs' large-page TLB errata), then the
/// table goes in; a thread that touches the range meanwhile takes a fault that waits for the caller's lock and
/// finds the entries. Lockless software walkers (`translate` from ksync's futex word access or the panic
/// backtrace) can see the slot empty for that moment; they treat it as not present (futex: `Fault`, then the
/// locked fault path and a retry). `NotMapped` (nothing changed, `pt` stays the caller's) when no 2 MiB leaf is
/// there.
pub fn split_user_2m(root: u64, virt: u64, pt: u64, cause: tlb::Cause) -> Result<Outcome, MapError> {
    let size = PageSize::Size2M.bytes();
    if virt >= crate::USER_TOP || !virt.is_multiple_of(size) || pt & !ADDR_MASK != 0 || pt == 0 {
        return Err(MapError::WrongRange);
    }
    let mut t = root;
    for level in [4, 3] {
        let e = read_entry(t, walk::index(virt, level));
        if e & PageFlags::PRESENT == 0 || e & PageFlags::HUGE != 0 {
            return Err(MapError::NotMapped);
        }
        t = e & ADDR_MASK;
    }
    let i = walk::index(virt, 2);
    let pde = read_entry(t, i);
    if pde & (PageFlags::PRESENT | PageFlags::HUGE) != PageFlags::PRESENT | PageFlags::HUGE {
        return Err(MapError::NotMapped);
    }
    let fill = |e: u64| {
        for j in 0..512 {
            // SAFETY: `pt` is a page-table frame the caller owns and nobody walks yet (RAM, direct-mapped); j < 512.
            unsafe { table(pt).add(j).write_volatile(walk::small_entry(e, j)) }
        }
    };
    fill(pde);
    // SAFETY: entry `i` of a live page directory of `root`'s user half (MM's own tables, aligned u64). Writers hold
    // the space lock (the caller's); concurrent readers are the hardware page walkers (atomic A/D updates, as in
    // `LeafWriter`) and lockless software walkers, which read through `read_entry`'s atomic load, so every
    // concurrent access is a whole-word atomic one and none can see a torn entry.
    let a = unsafe { core::sync::atomic::AtomicU64::from_ptr(table(t).add(i)) };
    let old = a.swap(0, core::sync::atomic::Ordering::AcqRel);
    if old != pde {
        // The hardware set accessed/dirty after the read (a store through the old entry): keep them.
        fill(old);
    }
    let o = tlb::flush_user_huge(root, virt, virt + size, cause);
    a.store(pt | PageFlags::PRESENT | PageFlags::WRITABLE | PageFlags::USER, core::sync::atomic::Ordering::Release);
    Ok(o)
}

/// The flag bits a user-half leaf may carry: never GLOBAL. A global translation survives CR3 and PCID switches,
/// so another address space could still reach this frame through a stale TLB entry.
fn user_bits(flags: PageFlags) -> u64 {
    flags.0 & !PageFlags::GLOBAL
}

/// The shootdown after one page of `root` changed: the kernel half on every CPU, a user page on the CPUs
/// that have `root` loaded.
fn flush_one(root: u64, virt: u64) -> Outcome {
    if virt >= KERNEL_HALF {
        tlb::flush_kernel(virt, virt + 4096, false)
    } else {
        tlb::flush_user(root, virt, virt + 4096, false, false, tlb::Cause::Other)
    }
}

/// Removes a 4 KiB mapping and returns the frame it pointed to once no CPU can reach it any more (shootdown
/// done). Tables stay. `None` when nothing was mapped, or when a CPU did not confirm the shootdown: then the
/// frame stays out of use (counted in `tlb::stats().kept_pages`). In the kernel half only the vmap range
/// (stacks, the CPU area, `reserve_kernel_va` windows) is accepted: the kernel image and the direct map are
/// never unmapped this way, so their frames cannot be handed back while still in use.
pub fn unmap_4k(root: u64, virt: u64) -> Option<u64> {
    if virt >= KERNEL_HALF && !kernel::in_vmap(virt & !0xFFF, 4096) {
        return None;
    }
    let p = leaf_ptr(root, virt)?;
    // SAFETY: leaf_ptr returns a pointer into a live page table.
    let e = unsafe { p.read_volatile() };
    if e & PageFlags::PRESENT == 0 {
        return None;
    }
    // SAFETY: as above.
    unsafe { p.write_volatile(0) };
    if !flush_one(root, virt).confirmed() {
        tlb::count_kept(1, 0);
        return None;
    }
    Some(e & ADDR_MASK)
}

/// Unmaps the 4 KiB pages of [virt, virt + len) in the kernel half of `root` (tables stay), shoots them down on
/// every CPU in one round (or one per `GATHER` pages), then hands each frame to `free`; frames of a round a CPU
/// did not confirm stay out of use (counted in `tlb::stats().kept_pages`). Returns how many pages were mapped.
/// For SC's `KernelStack::drop`: one shootdown per stack instead of one per page, no allocation for a stack.
/// Only the vmap range is accepted (as in `unmap_4k`); anything else returns 0 and changes nothing.
pub fn unmap_kernel_pages(root: u64, virt: u64, len: u64, free: &mut dyn FnMut(u64)) -> u64 {
    let Some(end) = virt.checked_add(len) else { return 0 };
    if virt & 0xFFF != 0 || len & 0xFFF != 0 || !kernel::in_vmap(virt, len) {
        return 0;
    }
    let mut g = Gather::for_pages(len / 4096);
    let mut give = |g: &mut Gather, ok: bool| {
        g.drain(&mut |(f, _, _)| {
            if ok {
                free(f)
            } else {
                tlb::count_kept(1, 0)
            }
        })
    };
    let (mut lo, mut n, mut va) = (virt, 0u64, virt);
    while va < end {
        if let Some(p) = leaf_ptr(root, va) {
            // SAFETY: leaf_ptr returns a pointer into a live page table.
            let e = unsafe { p.read_volatile() };
            if e & PageFlags::PRESENT != 0 {
                if g.full() {
                    let ok = tlb::flush_kernel(lo, va, false).confirmed();
                    give(&mut g, ok);
                    lo = va;
                }
                // SAFETY: as above.
                unsafe { p.write_volatile(0) };
                g.push((e & ADDR_MASK, PageSize::Size4K, 0));
                n += 1;
            }
        }
        va += 4096;
    }
    if n > 0 {
        let ok = tlb::flush_kernel(lo, end, false).confirmed();
        give(&mut g, ok);
    }
    n
}

fn leaf_ptr(root: u64, virt: u64) -> Option<*mut u64> {
    let mut t = root;
    for level in (2..=4).rev() {
        let e = read_entry(t, walk::index(virt, level));
        if e & PageFlags::PRESENT == 0 || e & PageFlags::HUGE != 0 {
            return None;
        }
        t = e & ADDR_MASK;
    }
    // SAFETY: index < 512 within a 4 KiB table.
    Some(unsafe { table(t).add(walk::index(virt, 1)) })
}

/// Translates `virt` in `root` (handles 1 GiB and 2 MiB pages). Returns the physical address and the leaf
/// flags in the 4 KiB layout; intermediate USER/WRITABLE bits are folded in (all levels must allow).
pub fn translate(root: u64, virt: u64) -> Option<(u64, PageFlags)> {
    walk::translate(&Reader, root, virt).map(|l| (l.phys, l.flags))
}

/// Like `translate`, with the page size and the page's first address.
pub fn translate_leaf(root: u64, virt: u64) -> Option<Leaf> {
    walk::translate(&Reader, root, virt)
}

/// Calls `f` for every present leaf of `root` in [lo, hi) (within one canonical half).
pub fn for_each_leaf(root: u64, lo: u64, hi: u64, f: &mut dyn FnMut(Unmapped)) {
    walk::for_each_leaf(&Reader, root, lo, hi, f)
}

/// The growth step of the frame list a user unmap gathers before it shoots them down and hands them back: the
/// list grows by this much while the heap allows, up to `tlb_plan::GATHER_MAX` frames (Linux's mmu_gather
/// bound); only a larger range, or no heap, takes several rounds (MM-tlb2: an 8 MiB munmap was 5 rounds, each a
/// whole-context flush on every other CPU of the mask). Without heap for the list, `GATHER_STACK` on the stack.
const GATHER: usize = 512;
const GATHER_STACK: usize = 32;

/// A gathered frame: its address, its size and the caller's tag (`UserGather::unmap`).
type Gathered = (u64, PageSize, u32);

struct Gather {
    heap: alloc::vec::Vec<Gathered>,
    stack: [Gathered; GATHER_STACK],
    n: usize,
    on_heap: bool,
}

impl Gather {
    fn new() -> Self {
        Self::for_pages(u64::MAX)
    }
    /// A list for at most `pages` frames: on the stack when they fit (no allocation, e.g. in the switch path).
    fn for_pages(pages: u64) -> Self {
        let mut heap = alloc::vec::Vec::new();
        let on_heap = pages > GATHER_STACK as u64 && heap.try_reserve_exact(GATHER).is_ok();
        Gather { heap, stack: [(0, PageSize::Size4K, 0); GATHER_STACK], n: 0, on_heap }
    }
    /// No room for another frame: the list is at `GATHER_MAX` or cannot grow (no heap).
    fn full(&mut self) -> bool {
        if !self.on_heap {
            return self.n >= GATHER_STACK;
        }
        let n = self.heap.len();
        n >= tlb_plan::GATHER_MAX || (n == self.heap.capacity() && self.heap.try_reserve(GATHER.min(tlb_plan::GATHER_MAX - n)).is_err())
    }
    fn push(&mut self, x: Gathered) {
        if self.on_heap {
            self.heap.push(x);
        } else {
            self.stack[self.n] = x;
            self.n += 1;
        }
    }
    fn drain(&mut self, f: &mut dyn FnMut(Gathered)) {
        if self.on_heap {
            self.heap.drain(..).for_each(f);
        } else {
            self.stack[..self.n].iter().copied().for_each(f);
            self.n = 0;
        }
    }
}

/// A frame or page table a user unmap took out of `root`. `confirmed`: every CPU dropped its translations,
/// free it; otherwise keep it out of use (a CPU did not answer the shootdown).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Released {
    pub phys: u64,
    pub size: PageSize,
    pub table: bool,
    pub confirmed: bool,
}

/// What `unmap_user` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UserUnmap {
    pub leaves: u64,
    pub tables_freed: u64,
    /// Shootdown rounds (one per `GATHER` frames, at least one when anything changed).
    pub shootdowns: u32,
    pub outcome: Outcome,
}

/// MM-11: one user unmap over one or more ranges (Linux's mmu_gather, linux-6.6/mm/mmu_gather.c): the leaves of
/// every `unmap` go into one list, and one shootdown round for all of them (`finish`) hands the frames back, then
/// the emptied page tables; only a list that cannot grow (`GATHER_MAX` frames, no heap) takes a round on the way
/// (`tlb_flush_mmu`). One munmap, madvise or hole punch over several VMAs is so one round, not one per VMA. The
/// round covers the span of the leaves actually removed; more than `tlb::RANGE_PAGES` pages, or freed tables,
/// flush the whole context (Linux `flush_tlb_mm_range`). Must be finished: what is gathered is handed back only
/// by a round.
pub struct UserGather {
    root: u64,
    cause: tlb::Cause,
    frames: Gather,
    tables: alloc::vec::Vec<u64>,
    /// The span of the leaves gathered since the last round (`lo >= hi`: none).
    lo: u64,
    hi: u64,
    /// MM-13: a 4 KiB leaf was gathered since the last round (none: only 2 MiB entries, one invlpg each).
    small: bool,
    leaves: u64,
    tables_freed: u64,
    out: Outcome,
    rounds: u32,
}

/// The gather and the caller's hand-back, shared by the leaf callback and the table hook of one walk.
struct Pending<'a, 'b> {
    g: &'a mut UserGather,
    release: &'a mut (dyn FnMut(Released, u32) + 'b),
}

impl UserGather {
    pub fn new(root: u64, cause: tlb::Cause) -> Self {
        UserGather {
            root,
            cause,
            frames: Gather::new(),
            tables: alloc::vec::Vec::new(),
            lo: u64::MAX,
            hi: 0,
            small: false,
            leaves: 0,
            tables_freed: 0,
            out: Outcome::default(),
            rounds: 0,
        }
    }

    /// Removes every leaf in [virt, virt + len) of the root's user half into the list, each frame with `tag`
    /// (handed back to `release` with it). A page table that became empty is unlinked and freed after the last
    /// round only when its whole span lies in [free.0, free.1): the gap between the neighbouring VMAs for a
    /// munmap (Linux `free_pgtables`' floor and ceiling, linux-6.6/mm/memory.c:301/363), an empty window for
    /// madvise and reclaim (Linux's `zap_page_range_single`, mm/memory.c:1750, frees none), so their round need
    /// not ask CPUs that hold the root lazily. Nothing changes on an error (`PartialHugePage`, a bad range).
    pub fn unmap(&mut self, virt: u64, len: u64, tag: u32, free: (u64, u64), release: &mut dyn FnMut(Released, u32)) -> Result<UnmapStats, MapError> {
        user_range(virt, len)?;
        let root = self.root;
        let p = core::cell::RefCell::new(Pending { g: &mut *self, release });
        let mut t = UserTables { p: &p, free };
        let st = walk::unmap(&mut t, root, virt, len, 4, &mut |u| {
            let mut b = p.borrow_mut();
            let Pending { g, release } = &mut *b;
            if g.frames.full() {
                tlb::count_gather_early();
                g.round(false, &mut **release);
            }
            g.frames.push((u.phys, u.size, tag));
            g.small |= u.size == PageSize::Size4K;
            g.lo = g.lo.min(u.virt);
            g.hi = g.hi.max(u.virt + u.size.bytes());
        })?;
        self.leaves += st.leaves;
        self.tables_freed += st.tables_freed;
        Ok(st)
    }

    /// The round for what is gathered: the frames, and with `tables` the emptied tables (a full flush that asks
    /// lazy CPUs too). Returns whether every CPU confirmed.
    fn round(&mut self, tables: bool, release: &mut dyn FnMut(Released, u32)) -> bool {
        if !tables && self.lo >= self.hi {
            return true;
        }
        let o = if tables {
            tlb::flush_user(self.root, 0, 0, true, true, self.cause)
        } else if !self.small {
            tlb::flush_user_huge(self.root, self.lo, self.hi, self.cause)
        } else {
            tlb::flush_user(self.root, self.lo, self.hi, false, false, self.cause)
        };
        self.out.add(o);
        self.rounds += 1;
        let ok = o.confirmed();
        self.frames.drain(&mut |(phys, size, tag)| release(Released { phys, size, table: false, confirmed: ok }, tag));
        if tables {
            for f in self.tables.drain(..) {
                release(Released { phys: f, size: PageSize::Size4K, table: true, confirmed: ok }, 0);
            }
        }
        (self.lo, self.hi, self.small) = (u64::MAX, 0, false);
        ok
    }

    /// A round now for the gathered frames (the emptied tables wait for `finish`); for a caller that must hand
    /// them back before it gathers more.
    pub fn flush(&mut self, release: &mut dyn FnMut(Released, u32)) -> bool {
        self.round(false, release)
    }

    /// The last round: every gathered frame, then every emptied table, goes to `release`.
    pub fn finish(mut self, release: &mut dyn FnMut(Released, u32)) -> UserUnmap {
        let tables = !self.tables.is_empty();
        self.round(tables, release);
        UserUnmap { leaves: self.leaves, tables_freed: self.tables_freed, shootdowns: self.rounds, outcome: self.out }
    }
}

/// User-half tables: entries through the direct map, emptied tables kept until the shootdown (a freed table
/// must not stay in a paging-structure cache: reused and filled with other data, it could yield a global
/// entry).
struct UserTables<'c, 'a, 'b> {
    p: &'c core::cell::RefCell<Pending<'a, 'b>>,
    free: (u64, u64),
}

impl walk::Tables for UserTables<'_, '_, '_> {
    fn read(&self, t: u64, i: usize) -> u64 {
        read_entry(t, i)
    }
    fn write(&mut self, t: u64, i: usize, v: u64) {
        // SAFETY: as in `Direct::write`; only user-half entries of a root MM owns are written here.
        unsafe { table(t).add(i).write_volatile(v) }
    }
    fn alloc_table(&mut self) -> Option<u64> {
        None
    }
    fn free_table(&mut self, t: u64) {
        let mut b = self.p.borrow_mut();
        let Pending { g, release } = &mut *b;
        if g.tables.try_reserve(1).is_ok() {
            g.tables.push(t);
        } else {
            // No room to defer: shoot the whole root down now (with what is gathered), then hand the table back.
            tlb::count_gather_early();
            let ok = g.round(true, &mut **release);
            release(Released { phys: t, size: PageSize::Size4K, table: true, confirmed: ok }, 0);
        }
    }
    fn may_free(&self, start: u64, len: u64) -> bool {
        start >= self.free.0 && start.checked_add(len).is_some_and(|e| e <= self.free.1)
    }
}

fn user_range(virt: u64, len: u64) -> Result<u64, MapError> {
    let end = virt.checked_add(len).ok_or(MapError::WrongRange)?;
    if virt & 0xFFF != 0 || len & 0xFFF != 0 {
        return Err(MapError::NotAligned);
    }
    if end > crate::USER_TOP {
        return Err(MapError::WrongRange);
    }
    Ok(end)
}

/// Removes every leaf in [virt, virt + len) of `root`'s user half and hands each frame and each page table
/// that became empty (PT, PD, PDPT; never the root) to `release` after the shootdown that covers it: one
/// `UserGather` round (more only beyond `GATHER_MAX` frames), the tables after the last (a full flush of the
/// root then). `Released::confirmed` says whether the frame may be reused.
pub fn unmap_user(root: u64, virt: u64, len: u64, cause: tlb::Cause, release: &mut dyn FnMut(Released)) -> Result<UserUnmap, MapError> {
    let mut g = UserGather::new(root, cause);
    let mut rel = |r: Released, _: u32| release(r);
    g.unmap(virt, len, 0, (0, u64::MAX), &mut rel)?;
    Ok(g.finish(&mut rel))
}

/// Replaces the present 4 KiB leaf at `virt` of `root`'s user half by `phys` with `flags` in one write and shoots
/// the old translation down (MM-5b: a copy-on-write copy, a device's new frame). Returns the old frame and the
/// shootdown's outcome: the caller may reuse the old frame only when it is confirmed. No table is allocated or
/// freed, so the swap cannot fail half-way. `NotMapped` when no 4 KiB leaf is present there (nothing changed).
pub fn replace_user_4k(root: u64, virt: u64, phys: u64, flags: PageFlags) -> Result<(u64, Outcome), MapError> {
    if virt >= crate::USER_TOP || virt & 0xFFF != 0 || phys & !ADDR_MASK != 0 {
        return Err(MapError::WrongRange);
    }
    let p = leaf_ptr(root, virt).ok_or(MapError::NotMapped)?;
    // SAFETY: leaf_ptr returns a pointer into a live user-half page table of `root` (MM's own tables).
    let e = unsafe { p.read_volatile() };
    if e & PageFlags::PRESENT == 0 {
        return Err(MapError::NotMapped);
    }
    // SAFETY: as above; a 4 KiB leaf (the PAT bit of `flags` is the 4 KiB layout's). GLOBAL is masked off
    // (`user_bits`), so a CR3 or PCID switch drops the translation and no other address space can use it.
    unsafe { p.write_volatile(phys | (user_bits(flags) & !ADDR_MASK) | PageFlags::PRESENT) };
    Ok((e & ADDR_MASK, tlb::flush_user(root, virt, virt + 4096, false, false, tlb::Cause::Cow)))
}

/// Leaf rewrites only (no table is allocated or freed).
/// Rewrites user leaves without losing the Accessed/Dirty bits another CPU's page walker sets between the read and
/// the write (it may still hold the old writable TLB entry until the shootdown): the write is a compare-exchange
/// against the value read, and bits the hardware set meanwhile are kept unless the new flags cleared them on
/// purpose (linux-6.6/mm/mprotect.c change_pte_range: ptep_modify_prot_start/commit).
struct LeafWriter {
    /// The last entry read: (table, index, value).
    last: core::cell::Cell<(u64, usize, u64)>,
}

const HW_BITS: u64 = PageFlags::ACCESSED | PageFlags::DIRTY;

/// The entry to store when `want` replaces `seen` (read before) and the table now holds `cur`: bits the hardware
/// set after the read are kept, unless `want` dropped them from `seen` on purpose.
fn merge_hw_bits(seen: u64, want: u64, cur: u64) -> u64 {
    let cleared = seen & !want & HW_BITS;
    want | (cur & HW_BITS & !cleared)
}

impl walk::Tables for LeafWriter {
    fn read(&self, t: u64, i: usize) -> u64 {
        let e = read_entry(t, i);
        self.last.set((t, i, e));
        e
    }
    fn write(&mut self, t: u64, i: usize, v: u64) {
        let (lt, li, le) = self.last.get();
        let seen = if (lt, li) == (t, i) { le } else { read_entry(t, i) };
        // SAFETY: as in `Direct::write`; only leaves of a user half MM owns are rewritten here. The entry is an
        // aligned u64 inside a live page table, and other CPUs touch it only through their page walkers (atomic
        // A/D updates), so viewing it as an AtomicU64 for the exchange is sound.
        let a = unsafe { core::sync::atomic::AtomicU64::from_ptr(table(t).add(i)) };
        let mut cur = seen;
        loop {
            match a.compare_exchange(cur, merge_hw_bits(seen, v, cur), core::sync::atomic::Ordering::AcqRel, core::sync::atomic::Ordering::Acquire) {
                Ok(_) => break,
                Err(now) => cur = now,
            }
        }
    }
    fn alloc_table(&mut self) -> Option<u64> {
        None
    }
    fn free_table(&mut self, _t: u64) {}
}

/// Sets the flags of every present leaf in [virt, virt + len) of `root`'s user half to what `flags` returns
/// for it (`None`: unchanged) and shoots the old ones down on every CPU that has `root` loaded. Returns how
/// many entries changed and the shootdown's outcome. A large leaf only partly inside the range gives
/// `PartialHugePage` and nothing changes.
/// `cause`: what asked for it (`tlb::stats().by_cause`).
pub fn protect_user(root: u64, virt: u64, len: u64, cause: tlb::Cause, flags: &mut dyn FnMut(Unmapped) -> Option<PageFlags>) -> Result<(u64, Outcome), MapError> {
    let end = user_range(virt, len)?;
    let (mut lo, mut hi, mut small) = (u64::MAX, 0u64, false);
    let n = walk::update_leaves(&mut LeafWriter { last: core::cell::Cell::new((0, usize::MAX, 0)) }, root, virt, end, &mut |u| {
        let nf = flags(u)?;
        lo = lo.min(u.virt);
        hi = hi.max(u.virt + u.size.bytes());
        small |= u.size == PageSize::Size4K;
        Some(user_bits(nf))
    })?;
    let out = if n > 0 { flush_leaves(root, lo, hi, small, cause) } else { Outcome::default() };
    Ok((n, out))
}

/// The round after leaves in [lo, hi) changed: `small` when a 4 KiB leaf is among them, else only 2 MiB
/// entries (MM-13: one invlpg each).
fn flush_leaves(root: u64, lo: u64, hi: u64, small: bool, cause: tlb::Cause) -> Outcome {
    if small { tlb::flush_user(root, lo, hi, false, false, cause) } else { tlb::flush_user_huge(root, lo, hi, cause) }
}

/// `protect_user` for the fault path, which only widens rights (a write to a read-only page that may be
/// written: copy-on-write reuse, a dirty mark of a shared file or device page, an entry behind its VMA): leaves
/// whose change only grants (`tlb_plan::grant_only`: WRITABLE set, NX cleared, Accessed/Dirty kept) are rewritten
/// without a shootdown round, as Linux's `wp_page_reuse` (linux-6.6/mm/memory.c:3030, `ptep_set_access_flags`
/// arch/x86/mm/pgtable.c:499: no flush); a CPU holding the narrower translation takes one spurious fault, which
/// flushes it there (`mm`'s fault path; a kernel copy needs `tlb::set_spurious_copy_retry`, else the round is
/// kept, `tlb::grants_allowed`). This CPU drops its own entries now when it has `root` loaded. A leaf
/// change that is more than a grant shoots the range down as `protect_user` does (MM-tlb2: the fbdev
/// deferred-io write faults of a 1080p picture were 2,025 rounds per frame, 55-78k IPIs/s per CPU).
pub fn grant_user(root: u64, virt: u64, len: u64, flags: &mut dyn FnMut(Unmapped) -> Option<PageFlags>) -> Result<(u64, Outcome), MapError> {
    const LOCAL: usize = tlb_plan::RANGE_PAGES as usize;
    let end = user_range(virt, len)?;
    let (mut lo, mut hi, mut wider, mut small) = (u64::MAX, 0u64, false, false);
    // The changed leaves this CPU drops with `invlpg` (one per leaf, a large one too); more than `LOCAL` take
    // the shootdown path.
    let (mut local, mut k) = ([0u64; LOCAL], 0usize);
    let n = walk::update_leaves(&mut LeafWriter { last: core::cell::Cell::new((0, usize::MAX, 0)) }, root, virt, end, &mut |u| {
        // The hardware's Accessed/Dirty bits stay: dropping them would need a flush (`grant_only`).
        let nf = user_bits(flags(u)?) | (u.flags.0 & HW_BITS);
        if nf != u.flags.0 {
            lo = lo.min(u.virt);
            hi = hi.max(u.virt + u.size.bytes());
            wider |= !tlb_plan::grant_only(u.phys | u.flags.0, u.phys | nf);
            small |= u.size == PageSize::Size4K;
            if k < LOCAL {
                local[k] = u.virt;
            }
            k += 1;
        }
        Some(nf)
    })?;
    if n == 0 {
        return Ok((0, Outcome::default()));
    }
    if wider || k > LOCAL {
        return Ok((n, flush_leaves(root, lo, hi, small, tlb::Cause::Fault)));
    }
    // MM-tlb2 fixer: a kernel copy (`read()` into the page) on a CPU that still holds the read-only entry
    // faults on an access the table allows; until the copy fault path retries that as spurious, the round stays.
    if !tlb::grants_allowed(root) {
        tlb::count_grant_rounds(n);
        return Ok((n, flush_leaves(root, lo, hi, small, tlb::Cause::Fault)));
    }
    if current_root() == root {
        local[..k].iter().for_each(|&va| flush_page(va));
    }
    tlb::count_grants(n);
    Ok((n, Outcome::default()))
}

/// How many page tables (PDPT, PD, PT) hang below `root`'s user half.
pub fn count_user_tables(root: u64) -> u64 {
    fn walk(t: u64, level: u32, first: usize, last: usize) -> u64 {
        let mut n = 0;
        for i in first..last {
            let e = read_entry(t, i);
            if e & PageFlags::PRESENT != 0 && e & PageFlags::HUGE == 0 && level > 1 {
                n += 1 + walk(e & ADDR_MASK, level - 1, 0, 512);
            }
        }
        n
    }
    walk(root, 4, 0, 256)
}

/// How many present leaves `root` has in [lo, hi) of its user half.
pub fn count_user_leaves(root: u64, lo: u64, hi: u64) -> u64 {
    let mut n = 0;
    walk::for_each_leaf(&Reader, root, lo, hi.min(crate::USER_TOP), &mut |_| n += 1);
    n
}

/// Gives every kernel-half PML4 slot (256..512) a PDPT now, so user roots that copy the kernel half see every
/// later kernel mapping (vmap, MMIO) without syncing. Costs 256 frames (1 MiB) once.
pub fn populate_kernel_half(root: u64, alloc: &mut dyn FnMut() -> Option<u64>) -> Result<(), MapError> {
    use walk::Tables;
    let mut free = |_| {};
    let mut d = Direct { alloc, free: &mut free };
    for i in 256..512 {
        if d.read(root, i) & PageFlags::PRESENT == 0 {
            let f = d.alloc_table().ok_or(MapError::NoMemory)?;
            d.write(root, i, f | PageFlags::PRESENT | PageFlags::WRITABLE);
        }
    }
    Ok(())
}

/// A new root for a process from `alloc_block`: two frames, 8 KiB aligned, nobody else's. The first becomes
/// the PML4 (empty user half, kernel half shared with `kernel_root`), the second the root's TLB context
/// (`tlb`). Give the block back only after a confirmed `tlb::leave`, the user half's teardown and
/// `retire_user_root`.
pub fn new_user_root(kernel_root: u64, alloc_block: &mut dyn FnMut() -> Option<u64>) -> Option<u64> {
    let f = alloc_block()?;
    if f & 0x1FFF != 0 {
        return None;
    }
    // SAFETY: a fresh 8 KiB block (the allocator's contract); the kernel root is live; all direct-mapped.
    unsafe {
        let dst = table(f);
        core::ptr::write_bytes(dst as *mut u8, 0, 2048);
        core::ptr::copy_nonoverlapping(table(kernel_root).add(256), dst.add(256), 256);
        let e = dst.add(tlb::MARK_ENTRY);
        e.write_volatile(e.read_volatile() | tlb::USER_ROOT_MARK);
    }
    tlb::init_ctx(f);
    Some(f)
}

/// Takes the mark and the context off a user root before its block is freed (see `new_user_root`).
pub fn retire_user_root(root: u64) {
    // Anything but a live user root (mark and magic) is left alone: no write into memory the caller named.
    if !tlb::has_context(root) {
        return;
    }
    tlb::retire_ctx(root);
    if crate::BARE_METAL {
        // SAFETY: `root` is a user root (live until the caller frees it), entry 256 of its PML4; only the
        // software bit the CPU ignores changes.
        unsafe {
            let e = table(root).add(tlb::MARK_ENTRY);
            e.write_volatile(e.read_volatile() & !tlb::USER_ROOT_MARK);
        }
    }
}

/// Calls `f(virt, phys, flags, size)` for every present user leaf below `USER_TOP` in `root` (4 KiB, and MM-13's
/// 2 MiB pages; flags in the 4 KiB layout), then frees the user-half tables through `free_table`. Used when a
/// process ends.
pub fn teardown_user_half(root: u64, f: &mut dyn FnMut(u64, u64, PageFlags, PageSize), free_table: &mut dyn FnMut(u64)) {
    fn walk(t: u64, level: u32, base: u64, f: &mut dyn FnMut(u64, u64, PageFlags, PageSize), free_table: &mut dyn FnMut(u64)) {
        let span = 1u64 << (12 + 9 * (level - 1));
        let end = if level == 4 { 256 } else { 512 };
        for i in 0..end {
            let e = read_entry(t, i);
            if e & PageFlags::PRESENT == 0 {
                continue;
            }
            let va = base + i as u64 * span;
            if level == 1 {
                f(va, e & ADDR_MASK, PageFlags(e & !ADDR_MASK), PageSize::Size4K);
            } else if e & PageFlags::HUGE == 0 {
                walk(e & ADDR_MASK, level - 1, va, f, free_table);
                free_table(e & ADDR_MASK);
            } else if level == 2 {
                f(va, walk::leaf_phys(e, 2), PageFlags(walk::leaf_flags(e, 2)), PageSize::Size2M);
            }
        }
    }
    walk(root, 4, 0, f, free_table);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protect_keeps_hardware_set_bits() {
        let (a, d, w) = (PageFlags::ACCESSED, PageFlags::DIRTY, PageFlags::WRITABLE);
        let seen = 0x1000 | PageFlags::PRESENT | w;
        // mprotect read-only while another CPU set D and A after the read: both stay.
        assert_eq!(merge_hw_bits(seen, seen & !w, seen | a | d), (seen & !w) | a | d);
        // Bits cleared on purpose (they were in the read value, the new flags drop them) stay cleared.
        assert_eq!(merge_hw_bits(seen | a | d, seen, seen | a | d), seen);
        // Nothing changed meanwhile: the new value as given.
        assert_eq!(merge_hw_bits(seen, seen & !w, seen), seen & !w);
    }

    #[test]
    fn cache_bits_round_trip() {
        for c in [CacheType::WriteBack, CacheType::WriteCombining, CacheType::UncachedMinus, CacheType::Uncached] {
            let f = PageFlags::kernel_data().with_cache(c);
            assert_eq!(f.cache(), Some(c));
            assert_eq!(CacheType::pat_index(f.0), c as usize);
        }
        assert_eq!(CacheType::from_bits(PageFlags::PAT_4K), None, "entries 4-7 are not used");
        let f = PageFlags(PageFlags::PAT_4K | PageFlags::PCD).with_cache(CacheType::WriteCombining);
        assert_eq!(f.0, PageFlags::PWT, "with_cache clears a stray PAT bit");
    }
}
