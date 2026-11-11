//! The kernel's own page tables (MM-2): a fresh root with every kernel-half PDPT present (user roots share
//! them), mapping and unmapping in the kernel half with table freeing, copying leaves from the loader's root,
//! the checked CR3 switch, and the device-window hook that `mmio` maps through. What goes where (direct map,
//! W^X image, vmap) is decided in `mm::kernel_tables`; this file only does what needs `unsafe`.

use super::{walk, CacheType, Direct, MapError, PageFlags, PageSize, Reader, UnmapStats, Unmapped, ADDR_MASK};
use walk::Tables as _;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// First address of the kernel (upper canonical) half.
pub const KERNEL_HALF: u64 = 0xFFFF_8000_0000_0000;

static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);
/// The root `new_kernel_root` built and nobody has loaded yet: the only one `map_kernel` may fill outside
/// vmap, `switch_kernel_root` may load and `free_unused_root` may free (a user root shares the kernel half's
/// PDPTs, so freeing any other root's tables would free the kernel's).
static CANDIDATE: AtomicU64 = AtomicU64::new(0);
static TABLE_ALLOC: AtomicUsize = AtomicUsize::new(0);
static TABLE_FREE: AtomicUsize = AtomicUsize::new(0);
/// One change of the kernel half's tables at a time, whoever asks: `map_4k` (device windows of `mmio`, stacks, the CPU
/// area, `reserve_kernel_va` ranges), `map_kernel` and `unmap_kernel` (MM's vmap, under its own mutex). They share PDs
/// and PTs: two walkers entering a new region would both install a table and one would lose its pages, and a walker
/// could write a leaf into a table `unmap_kernel` just emptied and took out. The innermost lock of the kernel: nothing
/// is taken and nothing waits under it (table frames come before it, `map_shared`), and it is held for one short piece
/// of work (`unmap_kernel` takes it per slice) with interrupts off, so a holder is never preempted and never owes a
/// shootdown for long. A waiter spins with interrupts as the caller had them: it must not hold up other CPUs' shootdowns
/// or this CPU's interrupts for what may be several holders in a row, and it holds nothing an interrupt could want.
/// Clearing a leaf in a table that stays (`unmap_4k`, `unmap_kernel_pages`) needs it not: a table with a present leaf is
/// not empty, so nobody frees it.
static TABLES: AtomicBool = AtomicBool::new(false);

/// CPUs in the spin of `with_tables_locked`: only `let_waiters_in` looks at it.
static TABLES_WAITERS: AtomicU32 = AtomicU32::new(0);
/// How long `let_waiters_in` leaves the lock free: long enough for a waiter's load, its `poll` and its attempt, about a
/// microsecond or two. A waiter that is not at its spin just then (preempted) costs each slice no more than this.
const HAND_OFF_SPINS: u32 = 128;

pub(super) fn with_tables_locked<R>(f: impl FnOnce() -> R) -> R {
    let mut waiting = false;
    loop {
        let irq = crate::cpu::IrqGuard::new();
        if TABLES.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            if waiting {
                TABLES_WAITERS.fetch_sub(1, Ordering::Relaxed);
            }
            let r = f();
            TABLES.store(false, Ordering::Release);
            return r;
        }
        drop(irq);
        if !waiting {
            waiting = true;
            TABLES_WAITERS.fetch_add(1, Ordering::Relaxed);
        }
        while TABLES.load(Ordering::Relaxed) {
            // The caller may have interrupts off, and another CPU may be waiting for this one's answer to its shootdown.
            super::tlb::poll();
            core::hint::spin_loop();
        }
    }
}

/// Between two slices of one walk: this CPU would otherwise retake the lock before a waiter's load has even seen it free,
/// and a waiter would wait out the whole walk, a masked one with its interrupts off. Costs one load when nobody waits.
pub(super) fn let_waiters_in() {
    // A caller with interrupts off answers shootdowns only here for the length of the walk.
    super::tlb::poll();
    if TABLES_WAITERS.load(Ordering::Relaxed) == 0 {
        return;
    }
    for _ in 0..HAND_OFF_SPINS {
        if TABLES.load(Ordering::Relaxed) || TABLES_WAITERS.load(Ordering::Relaxed) == 0 {
            return;
        }
        core::hint::spin_loop();
    }
}

/// `walk::map` of one kernel-half leaf under `TABLES`. A table frame may wait for memory (`alloc` can reclaim), so the
/// walk runs with what it has and comes back for a frame only when it finds a table missing; each pass leaves the tables
/// it could install, so this ends. A frame another walker made unneeded goes back through the table hook. Once the hook
/// is installed it is the source of every frame here, whatever `alloc` the caller brought: that frame can go back, and a
/// table in the vmap area may later be freed through the hook by `unmap_kernel`, whoever installed it. `alloc` serves
/// the boot before the hook, on one CPU: no other walker, so no frame is left over there.
pub(super) fn map_shared(root: u64, virt: u64, phys: u64, size: PageSize, flags: u64, alloc: &mut dyn FnMut() -> Option<u64>) -> Result<(), MapError> {
    let hook = table_hook();
    let mut source = || match hook {
        Some(h) => h(),
        None => alloc(),
    };
    let (out, spare) = map_with_retries(&mut source, &mut |take| {
        let mut free = |_| {};
        let mut d = Direct { alloc: take, free: &mut free };
        with_tables_locked(|| walk::map(&mut d, root, virt, phys, size, flags))
    });
    if let (Some(f), true) = (spare, hook.is_some()) {
        let _ = with_frames(|d| d.free_table(f));
    }
    out
}

/// The passes of `map_shared`: `pass` is one walk that takes table frames from what it is given. Returns its result and
/// the frame `source` gave for a table another walker installed in the meantime, which the caller still owns.
pub(super) fn map_with_retries(
    source: &mut dyn FnMut() -> Option<u64>,
    pass: &mut dyn FnMut(&mut dyn FnMut() -> Option<u64>) -> Result<(), MapError>,
) -> (Result<(), MapError>, Option<u64>) {
    let mut spare = None;
    let out = loop {
        let mut short = false;
        let mut take = || {
            let f = spare.take();
            short = f.is_none();
            f
        };
        let out = pass(&mut take);
        if out != Err(MapError::NoMemory) || !short {
            break out;
        }
        spare = source();
        if spare.is_none() {
            break out;
        }
    };
    (out, spare)
}

/// The allocator `set_table_frames` installed.
fn table_hook() -> Option<fn() -> Option<u64>> {
    match TABLE_ALLOC.load(Ordering::Acquire) {
        0 => None,
        // SAFETY: only `set_table_frames` stores a non-zero value here, once, always a valid fn pointer of this type.
        a => Some(unsafe { core::mem::transmute::<usize, fn() -> Option<u64>>(a) }),
    }
}

/// The root `new_kernel_root` built and nobody has loaded yet (0: none).
pub(super) fn candidate() -> u64 {
    CANDIDATE.load(Ordering::Acquire)
}

/// The root `switch_kernel_root` installed (`None` while the loader's tables are in use).
pub fn kernel_root() -> Option<u64> {
    match KERNEL_ROOT.load(Ordering::Acquire) {
        0 => None,
        r => Some(r),
    }
}

/// Where kernel page-table frames come from and go back to (mm-core, kind PageTable). One-shot: the first
/// call installs the pair, later calls change nothing and return false (a second source could hand out frames
/// the first still owns, and `with_frames` must always see a matching pair).
pub fn set_table_frames(alloc: fn() -> Option<u64>, free: fn(u64)) -> bool {
    // `free` claims the slot; `alloc` is stored after it and is what `with_frames` checks first, so a reader
    // that sees `alloc` also sees the `free` of the same call.
    if TABLE_FREE.compare_exchange(0, free as usize, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return false;
    }
    TABLE_ALLOC.store(alloc as usize, Ordering::Release);
    true
}

pub fn table_frames_ready() -> bool {
    TABLE_ALLOC.load(Ordering::Acquire) != 0
}

pub(super) fn with_frames<R>(f: impl FnOnce(&mut Direct) -> R) -> Result<R, MapError> {
    let a = TABLE_ALLOC.load(Ordering::Acquire);
    let fr = TABLE_FREE.load(Ordering::Acquire);
    if a == 0 || fr == 0 {
        return Err(MapError::WrongRange);
    }
    // SAFETY: only `set_table_frames` stores non-zero values here, once each, always valid fn pointers of these
    // types (and of the same call: see there).
    let (a, fr) = unsafe { (core::mem::transmute::<usize, fn() -> Option<u64>>(a), core::mem::transmute::<usize, fn(u64)>(fr)) };
    let mut alloc = a;
    let mut free = fr;
    Ok(f(&mut Direct { alloc: &mut alloc, free: &mut free }))
}

/// A fresh PML4 with a PDPT in every kernel-half slot (so user roots that copy slots 256..512 see every later
/// kernel mapping). Not active; `free_unused_root` gives it back if the build fails.
pub fn new_kernel_root() -> Result<u64, MapError> {
    with_frames(|d| {
        let root = d.alloc_table().ok_or(MapError::NoMemory)?;
        for i in 256..512 {
            let Some(f) = d.alloc_table() else {
                free_tree(d, root, 4);
                return Err(MapError::NoMemory);
            };
            d.write(root, i, f | PageFlags::PRESENT | PageFlags::WRITABLE);
        }
        CANDIDATE.store(root, Ordering::Release);
        Ok(root)
    })?
}

/// Frees every table of the root `new_kernel_root` built if it was never loaded (a failed build).
pub fn free_unused_root(root: u64) -> Result<(), MapError> {
    if root == 0 || CANDIDATE.compare_exchange(root, 0, Ordering::AcqRel, Ordering::Acquire).is_err() || super::current_root() == root {
        return Err(MapError::WrongRange);
    }
    with_frames(|d| free_tree(d, root, 4))
}

pub(super) fn in_vmap(virt: u64, len: u64) -> bool {
    use bootinfo::layout::{VMAP_BASE, VMAP_END};
    virt >= VMAP_BASE && virt.checked_add(len).is_some_and(|e| e <= VMAP_END)
}

fn free_tree(d: &mut Direct, table: u64, level: u32) {
    if level > 1 {
        for i in 0..512 {
            let e = d.read(table, i);
            if e & PageFlags::PRESENT != 0 && (e & PageFlags::HUGE == 0 || level == 4) {
                free_tree(d, e & ADDR_MASK, level - 1);
            }
        }
    }
    d.free_table(table);
}

/// Maps one leaf of `size` (flags in the 4 KiB layout): anywhere in the kernel half of the root
/// `new_kernel_root` is building, only in the vmap area of the live kernel root, nowhere else.
pub fn map_kernel(root: u64, virt: u64, phys: u64, size: PageSize, flags: PageFlags) -> Result<(), MapError> {
    let building = root != 0 && CANDIDATE.load(Ordering::Acquire) == root;
    let live = kernel_root() == Some(root) && in_vmap(virt, size.bytes());
    if virt < KERNEL_HALF || flags.contains(PageFlags::USER) || !(building || live) {
        return Err(MapError::WrongRange);
    }
    // Without PDPE1GB the PS bit of a PDPTE is reserved: every access through it would fault (RSVD).
    if size == PageSize::Size1G && !crate::cpu::detect().pdpe1gb {
        return Err(MapError::Unsupported);
    }
    if !table_frames_ready() {
        return Err(MapError::WrongRange);
    }
    map_shared(root, virt, phys, size, flags.0, &mut || None)
}

/// Copies every leaf of `from` in [lo, hi) (kernel half) into `to` (the root being built) with the same
/// frame, size and flags, and returns them. Used for mappings made before the switch (early MMIO, the CPU
/// area's IST stacks).
pub fn adopt_leaves(from: u64, to: u64, lo: u64, hi: u64) -> Result<Vec<Unmapped>, MapError> {
    if lo < KERNEL_HALF {
        return Err(MapError::WrongRange);
    }
    let mut leaves = Vec::new();
    walk::for_each_leaf(&Reader, from, lo, hi, &mut |u| leaves.push(u));
    for u in &leaves {
        map_kernel(to, u.virt, u.phys, u.size, u.flags)?;
    }
    Ok(leaves)
}

/// Unmaps [virt, virt + len) of the vmap area in the kernel root and frees the PTs and PDs that became empty
/// (the shared PDPTs stay) once every CPU dropped the old translations (`tlb::flush_kernel`, also when nothing
/// was mapped: a retry after an unconfirmed attempt). `f` sees each removed leaf, with `TABLES` held and interrupts off
/// (it must not wait or allocate); what it pointed to must not be reused before this returns `Ok`. `NeedsShootdown`: a CPU did not confirm in time; the leaves are gone, but
/// the range must stay out of use, and the emptied tables are kept (counted in `tlb::stats().kept_tables`).
///
/// A whole reservation may be GiBs (a GPU buffer): `TABLES` is taken per slice of the walk (`walk::UnmapSlices`), so no
/// CPU spends the whole walk with interrupts off, and waiters get their turn between the slices (`let_waiters_in`). Between
/// the slices other walkers go on in the rest of the kernel half; the range itself is the caller's until this returns. A
/// table that reaches past the range is taken out only at the very end: until the shootdown another CPU may still hold its
/// entry in a paging-structure cache, and a neighbour could already be mapping into the slot with a new table. Interrupts
/// stay off from that moment to the shootdown, so this thread cannot be preempted in between; the window is then the
/// shootdown itself, up to `tlb`'s give-up time with a stuck CPU (plan/notes/PC-PC-W31.md, open point 1).
pub fn unmap_kernel(virt: u64, len: u64, f: &mut dyn FnMut(Unmapped)) -> Result<UnmapStats, MapError> {
    let Some(root) = kernel_root().filter(|_| in_vmap(virt, len)) else {
        return Err(MapError::WrongRange);
    };
    let mut slices = walk::UnmapSlices::new(virt, len)?;
    // Clear, shoot down, then free: emptied tables are only collected during the walk and go back to the frame
    // allocator after every CPU flushed, so no cached PDE anywhere can point at a reused frame.
    let mut freed = Vec::new();
    let mut lost = 0u64;
    // The room comes first (no heap under `TABLES`): at most the PTs and PDs the range touches.
    reserve_room(&mut freed, ((len >> 21) + (len >> 30) + 4) as usize);
    let mut st = UnmapStats::default();
    // The common small range (a stack, a window) is one step and one lock hold, tables that reach past it included.
    let one_step = slices.one_step();
    let mut to_shootdown = one_step.then(crate::cpu::IrqGuard::new);
    with_frames(|d| {
        let mut t = Deferred { d, freed: &mut freed, lost: &mut lost, range: (virt, virt + len), ends: one_step };
        with_tables_locked(|| {
            slices.check(&t, root)?;
            slices.step(&mut t, root, 3, f, &mut st);
            Ok::<(), MapError>(())
        })?;
        while !slices.done() {
            let_waiters_in();
            with_tables_locked(|| slices.step(&mut t, root, 3, f, &mut st));
        }
        if !one_step {
            let_waiters_in();
            to_shootdown = Some(crate::cpu::IrqGuard::new());
            t.ends = true;
            with_tables_locked(|| slices.finish(&mut t, root, 3, &mut st));
        }
        Ok::<(), MapError>(())
    })??;
    let o = super::tlb::flush_kernel_as(virt, virt + len, !freed.is_empty() || lost > 0, super::tlb::Cause::Vmap);
    drop(to_shootdown);
    if !o.confirmed() {
        super::tlb::count_kept(0, freed.len() as u64 + lost);
        return Err(MapError::NeedsShootdown);
    }
    super::tlb::count_kept(0, lost);
    with_frames(|d| {
        for t in freed {
            d.free_table(t);
        }
    })?;
    Ok(st)
}

/// Room for `want` more entries, or as many as the heap gives: what does not fit is lost one table at a time
/// (`Deferred`), not all of them.
pub(super) fn reserve_room(v: &mut Vec<u64>, want: usize) {
    let mut n = want;
    while n > 0 && v.try_reserve_exact(n).is_err() {
        n /= 2;
    }
}

/// A `walk::Tables` with table frees collected instead of done (see `unmap_kernel`).
pub(super) struct Deferred<'a, T> {
    pub(super) d: &'a mut T,
    pub(super) freed: &'a mut Vec<u64>,
    /// Emptied tables with no room in `freed`: kept out of use for good (never an allocation here, `TABLES` is held).
    pub(super) lost: &'a mut u64,
    /// What is being unmapped: tables wholly inside are nobody else's.
    pub(super) range: (u64, u64),
    /// The last piece of the walk: tables that reach past `range` may go too.
    pub(super) ends: bool,
}

impl<T: walk::Tables> walk::Tables for Deferred<'_, T> {
    fn read(&self, t: u64, i: usize) -> u64 {
        self.d.read(t, i)
    }
    fn write(&mut self, t: u64, i: usize, v: u64) {
        self.d.write(t, i, v)
    }
    fn alloc_table(&mut self) -> Option<u64> {
        self.d.alloc_table()
    }
    fn free_table(&mut self, t: u64) {
        if self.freed.len() < self.freed.capacity() {
            self.freed.push(t)
        } else {
            *self.lost += 1;
        }
    }
    fn may_free(&self, start: u64, len: u64) -> bool {
        self.ends || (start >= self.range.0 && start.saturating_add(len) <= self.range.1)
    }
}

/// [start, end) of the kernel image as linked (`__kernel_start`, `__kernel_end` from kernel/linker.ld).
pub fn kernel_image() -> (u64, u64) {
    #[cfg(target_os = "none")]
    {
        unsafe extern "C" {
            static __kernel_start: u8;
            static __kernel_end: u8;
        }
        (&raw const __kernel_start as u64, &raw const __kernel_end as u64)
    }
    #[cfg(not(target_os = "none"))]
    (0, 0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwitchError {
    /// A kernel-image page maps to another frame (or not at all) in the new root.
    ImageMismatch(u64),
    /// The running stack is not mapped to the same frames in the new root, or not writable there.
    StackMismatch(u64),
    /// A page loses a right the running kernel needs: an image page the loader mapped executable (and not
    /// writable) is NX, or writable (and not executable) is read-only; or the running code, an IDT gate target
    /// or the IDT itself is not mapped to the same frame and executable / present in the new root.
    RightsMismatch(u64),
    /// The new root is the loaded one already.
    SameRoot,
    /// Not the root `new_kernel_root` built (or it was loaded or freed already).
    NotBuilt,
}

/// Loads `new_root` after checking that the kernel image, the running stack, the running code and every IDT
/// gate map to the same frames in it with the rights the running kernel needs, flushes the TLB including
/// global entries, and records it as the kernel root. Interrupts are off meanwhile.
pub fn switch_kernel_root(new_root: u64) -> Result<(), SwitchError> {
    let old = super::current_root();
    if old == new_root {
        return Err(SwitchError::SameRoot);
    }
    if new_root == 0 || CANDIDATE.load(Ordering::Acquire) != new_root {
        return Err(SwitchError::NotBuilt);
    }
    let leaf = |root: u64, va: u64| walk::translate(&Reader, root, va);
    let same = |va: u64| match (leaf(old, va), leaf(new_root, va)) {
        (Some(a), Some(b)) => a.phys == b.phys,
        (None, _) => true,
        (Some(_), None) => false,
    };
    let rights = |l: &walk::Leaf| (l.flags.contains(PageFlags::WRITABLE), !l.flags.contains(PageFlags::NX));
    let (start, end) = kernel_image();
    let mut va = start & !0xFFF;
    while va < end {
        let (Some(a), Some(b)) = (leaf(old, va), leaf(new_root, va)) else {
            return Err(SwitchError::ImageMismatch(va));
        };
        if a.phys != b.phys {
            return Err(SwitchError::ImageMismatch(va));
        }
        // Only a page the loader mapped W+X may lose a right (the caller split it by the linker's sections);
        // any other page keeps what the running kernel has relied on so far.
        let ((ow, ox), (nw, nx)) = (rights(&a), rights(&b));
        if !(ow && ox) && ((ox && !nx) || (ow && !nw)) {
            return Err(SwitchError::RightsMismatch(va));
        }
        va += 0x1000;
    }
    // Executable at the same frame: the code running now and every gate the CPU may enter after the switch.
    let exec_same = |va: u64| match (leaf(old, va), leaf(new_root, va)) {
        (Some(a), Some(b)) => a.phys == b.phys && rights(&b).1,
        _ => false,
    };
    let rip = instruction_pointer();
    if !exec_same(rip) {
        return Err(SwitchError::RightsMismatch(rip));
    }
    let mut bad_gate = None;
    for_each_gate(&mut |page, target| {
        if bad_gate.is_none() && (!same(page) || leaf(new_root, page).is_none()) {
            bad_gate = Some(page);
        }
        if bad_gate.is_none() && !exec_same(target) {
            bad_gate = Some(target);
        }
    });
    if let Some(va) = bad_gate {
        return Err(SwitchError::RightsMismatch(va));
    }
    let _irq = crate::cpu::IrqGuard::new();
    let sp = stack_pointer() & !0xFFF;
    for p in 0..4 {
        let a = sp + p * 0x1000;
        let writable = match (leaf(old, a), leaf(new_root, a)) {
            (Some(o), Some(n)) => !rights(&o).0 || rights(&n).0,
            _ => true,
        };
        if !same(a) || !writable {
            return Err(SwitchError::StackMismatch(a));
        }
    }
    super::activate(new_root);
    flush_all();
    KERNEL_ROOT.store(new_root, Ordering::Release);
    CANDIDATE.store(0, Ordering::Release);
    Ok(())
}

fn instruction_pointer() -> u64 {
    #[cfg(target_os = "none")]
    {
        let ip: u64;
        // SAFETY: computes an address, no memory access.
        unsafe { core::arch::asm!("lea {}, [rip]", out(reg) ip, options(nomem, nostack, preserves_flags)) };
        ip
    }
    #[cfg(not(target_os = "none"))]
    0
}

/// Calls `f(idt_page, target)` for every present gate of the loaded IDT (`idt_page` is the page holding it).
fn for_each_gate(f: &mut dyn FnMut(u64, u64)) {
    #[cfg(target_os = "none")]
    {
        #[repr(C, packed)]
        struct Dtr {
            limit: u16,
            base: u64,
        }
        let mut d = Dtr { limit: 0, base: 0 };
        // SAFETY: sidt stores the 10-byte IDTR into `d`; nothing else is touched.
        unsafe { core::arch::asm!("sidt [{}]", in(reg) &mut d, options(nostack, preserves_flags)) };
        let (base, limit) = ({ d.base }, { d.limit } as u64);
        for i in 0..(limit + 1) / 16 {
            let g = base + i * 16;
            // SAFETY: a 16-byte gate inside the loaded IDT (bounded by its limit); the CPU uses the IDT, so it
            // is mapped; unaligned reads because nothing promises the base's alignment.
            let (lo, hi) = unsafe { ((g as *const u64).read_unaligned(), ((g + 8) as *const u64).read_unaligned()) };
            if lo & (1 << 47) == 0 {
                continue;
            }
            let target = (lo & 0xFFFF) | ((lo >> 32) & 0xFFFF_0000) | ((hi & 0xFFFF_FFFF) << 32);
            f(g & !0xFFF, target);
        }
    }
    #[cfg(not(target_os = "none"))]
    let _ = f;
}

fn stack_pointer() -> u64 {
    #[cfg(target_os = "none")]
    {
        let sp: u64;
        // SAFETY: reads rsp, no side effects.
        unsafe { core::arch::asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack, preserves_flags)) };
        sp
    }
    #[cfg(not(target_os = "none"))]
    0
}

/// Flushes this CPU's whole TLB, global entries and every PCID included (INVPCID type 2, or CR4.PGE off and
/// on, SDM 4.10.4.1). Other CPUs: `tlb::flush_kernel_all`.
pub fn flush_all() {
    super::tlb::flush_everything()
}

// ---------------------------------------------------------------------------------------------------------
// Device windows: how `mmio` gets a typed mapping from MM's vmap allocator without depending on `mm`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmapError {
    /// MM has not installed its allocator yet (before the switch to the kernel's own tables).
    NotReady,
    /// The range touches RAM (a device window must never alias the direct map with another type).
    OverlapsRam,
    /// The vmap area has no hole of this size.
    NoVirtualSpace,
    /// No frame for a page table.
    NoMemory,
    /// Zero length, overflow, or not a device range.
    BadRange,
    Map(MapError),
    /// The mapper's result does not map the asked range with the asked type (a bug in the mapper).
    Verify,
}

/// Maps `len` bytes of device memory at `phys` (any alignment) with `cache` in the kernel root; returns the
/// virtual address of `phys` and an owner whose drop unmaps the window.
pub type DeviceMapper = fn(phys: u64, len: u64, cache: CacheType) -> Result<(u64, Box<dyn Any + Send + Sync>), VmapError>;

static DEVICE_MAPPER: AtomicUsize = AtomicUsize::new(0);

/// MM installs its vmap allocator here when the kernel's own root is live. One-shot: later calls change
/// nothing and return false.
pub fn set_device_mapper(m: DeviceMapper) -> bool {
    DEVICE_MAPPER.compare_exchange(0, m as usize, Ordering::AcqRel, Ordering::Acquire).is_ok()
}

/// A mapped device window. Only `map_device_window` makes one, after checking the page tables; dropping it
/// unmaps the window (through the owner the mapper returned).
pub struct DeviceWindow {
    virt: u64,
    phys: u64,
    len: u64,
    cache: CacheType,
    _owner: Box<dyn Any + Send + Sync>,
}

impl DeviceWindow {
    pub fn virt(&self) -> u64 {
        self.virt
    }
    pub fn phys(&self) -> u64 {
        self.phys
    }
    pub fn len(&self) -> u64 {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn cache(&self) -> CacheType {
        self.cache
    }
}

impl core::fmt::Debug for DeviceWindow {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "DeviceWindow({:#x} -> {:#x}, {:#x} bytes, {:?})", self.virt, self.phys, self.len, self.cache)
    }
}

/// Maps a device range through the installed mapper and verifies the result in the kernel root: every page
/// present, writable, NX, not user, mapping exactly `phys..phys+len` with `cache`.
pub fn map_device_window(phys: u64, len: u64, cache: CacheType) -> Result<DeviceWindow, VmapError> {
    let m = DEVICE_MAPPER.load(Ordering::Acquire);
    if m == 0 {
        return Err(VmapError::NotReady);
    }
    // SAFETY: only `set_device_mapper` stores a non-zero value here, once, always a valid `DeviceMapper`.
    let m: DeviceMapper = unsafe { core::mem::transmute::<usize, DeviceMapper>(m) };
    let end = phys.checked_add(len).filter(|_| len != 0).ok_or(VmapError::BadRange)?;
    let (virt, owner) = m(phys, len, cache)?;
    let root = kernel_root().ok_or(VmapError::NotReady)?;
    let (first, last) = (phys & !0xFFF, (end + 0xFFF) & !0xFFF);
    let vbase = virt.wrapping_sub(phys - first);
    let mut off = 0;
    while first + off < last {
        let l = walk::translate(&Reader, root, vbase + off).ok_or(VmapError::Verify)?;
        let want = PageFlags::WRITABLE | PageFlags::NX;
        if l.phys != first + off || !l.flags.contains(want) || l.flags.contains(PageFlags::USER) || l.flags.cache() != Some(cache) {
            return Err(VmapError::Verify);
        }
        // Skip the rest of a large leaf.
        off = l.page_virt.wrapping_add(l.size.bytes()).wrapping_sub(vbase);
    }
    Ok(DeviceWindow { virt, phys, len, cache, _owner: owner })
}

/// Whether a mapper is installed.
pub fn device_mapper() -> bool {
    DEVICE_MAPPER.load(Ordering::Acquire) != 0
}
