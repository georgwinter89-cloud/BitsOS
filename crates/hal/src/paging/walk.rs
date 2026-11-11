//! The 4-level walk itself, over any `Tables` backend: the direct map in the kernel, a frame arena in host
//! tests. Mapping with 4 KiB / 2 MiB / 1 GiB leaves, translation, and unmapping that frees tables that became
//! empty. Leaf flags are always given and returned in the 4 KiB layout (PAT in bit 7); for large leaves the
//! PAT bit moves to bit 12 here, so callers never see the difference.

use super::{MapError, PageFlags, ADDR_MASK};

/// Where page-table frames live. `read`/`write` access entry `i` (< 512) of the table at physical `table`.
pub trait Tables {
    fn read(&self, table: u64, i: usize) -> u64;
    fn write(&mut self, table: u64, i: usize, v: u64);
    /// A zeroed table frame.
    fn alloc_table(&mut self) -> Option<u64>;
    fn free_table(&mut self, table: u64);
    /// Whether an emptied table that maps [start, start + len) may be freed (MM-11: a user unmap frees only tables
    /// no neighbouring VMA shares, Linux's `free_pgtables` floor and ceiling; madvise and reclaim free none).
    fn may_free(&self, _start: u64, _len: u64) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PageSize {
    Size4K,
    Size2M,
    Size1G,
}

impl PageSize {
    pub const fn bytes(self) -> u64 {
        match self {
            PageSize::Size4K => 1 << 12,
            PageSize::Size2M => 1 << 21,
            PageSize::Size1G => 1 << 30,
        }
    }
    /// Table level that holds a leaf of this size (1 = PT, 2 = PD, 3 = PDPT).
    pub const fn level(self) -> u32 {
        match self {
            PageSize::Size4K => 1,
            PageSize::Size2M => 2,
            PageSize::Size1G => 3,
        }
    }
    fn from_level(level: u32) -> PageSize {
        match level {
            1 => PageSize::Size4K,
            2 => PageSize::Size2M,
            _ => PageSize::Size1G,
        }
    }
}

/// A present leaf found by `translate`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    /// Physical address of the translated byte.
    pub phys: u64,
    /// First virtual address of the page.
    pub page_virt: u64,
    pub size: PageSize,
    /// Leaf flags in the 4 KiB layout. USER and WRITABLE are folded with the upper levels (all must allow).
    pub flags: PageFlags,
}

/// PAT bit of a 2 MiB / 1 GiB leaf.
pub const PAT_LARGE: u64 = 1 << 12;

pub(crate) fn index(virt: u64, level: u32) -> usize {
    ((virt >> (12 + 9 * (level - 1))) & 0x1FF) as usize
}

pub(super) fn span(level: u32) -> u64 {
    1u64 << (12 + 9 * (level - 1))
}

/// Leaf entry bits of `flags` (4 KiB layout) for a leaf at `level`.
pub(super) fn leaf_bits(flags: u64, level: u32) -> u64 {
    // Bit 7 is PAT in a 4 KiB leaf and the page-size bit above; bit 12 is address in a 4 KiB leaf.
    let f = flags & !ADDR_MASK & !PAT_LARGE;
    if level == 1 {
        return f | PageFlags::PRESENT;
    }
    let pat = if flags & PageFlags::PAT_4K != 0 { PAT_LARGE } else { 0 };
    (f & !PageFlags::PAT_4K) | pat | PageFlags::HUGE | PageFlags::PRESENT
}

/// The 4 KiB-layout flags of a leaf entry at `level`.
pub(super) fn leaf_flags(e: u64, level: u32) -> u64 {
    if level == 1 {
        return e & !ADDR_MASK;
    }
    let pat = if e & PAT_LARGE != 0 { PageFlags::PAT_4K } else { 0 };
    (e & !ADDR_MASK & !PageFlags::HUGE) | pat
}

/// MM-13: entry `i` (0..512) of the page table that maps what the 2 MiB leaf entry `pde` maps, 4 KiB at a time:
/// the frame at offset i * 4 KiB with the same rights, memory type and accessed/dirty bits (Linux
/// `__split_huge_pmd_locked`, linux-6.6/mm/huge_memory.c:2066, copies them into every pte).
pub fn small_entry(pde: u64, i: usize) -> u64 {
    (leaf_phys(pde, 2) + i as u64 * 4096) | leaf_bits(leaf_flags(pde, 2), 1)
}

/// Physical frame of a leaf entry at `level`.
pub(super) fn leaf_phys(e: u64, level: u32) -> u64 {
    e & ADDR_MASK & !(span(level) - 1)
}

/// Maps one page of `size` at `virt` -> `phys`. Intermediate tables are created as needed (writable, and
/// USER when the leaf is). Refuses a present leaf (`AlreadyMapped`) and a large page in the way.
pub fn map(t: &mut impl Tables, root: u64, virt: u64, phys: u64, size: PageSize, flags: u64) -> Result<(), MapError> {
    map_in(t, root, virt, phys, size, flags, flags & PageFlags::USER != 0)
}

/// As `map`; `user` gives the intermediate entries the USER bit whatever the leaf says, as Linux does for
/// every user-half table: a leaf mapped kernel-only first (PROT_NONE) and made user later by `update_leaves`
/// must not stay behind a table entry without USER.
pub fn map_in(t: &mut impl Tables, root: u64, virt: u64, phys: u64, size: PageSize, flags: u64, user: bool) -> Result<(), MapError> {
    let bytes = size.bytes();
    if !virt.is_multiple_of(bytes) || !phys.is_multiple_of(bytes) || phys & !ADDR_MASK != 0 {
        return Err(MapError::NotAligned);
    }
    let mut table = root;
    for level in ((size.level() + 1)..=4).rev() {
        let i = index(virt, level);
        let e = t.read(table, i);
        if e & PageFlags::PRESENT == 0 {
            let f = t.alloc_table().ok_or(MapError::NoMemory)?;
            let mut ne = f | PageFlags::PRESENT | PageFlags::WRITABLE;
            if user {
                ne |= PageFlags::USER;
            }
            t.write(table, i, ne);
            table = f;
        } else {
            if e & PageFlags::HUGE != 0 && level <= 3 {
                return Err(MapError::HugePageInTheWay);
            }
            if user && e & PageFlags::USER == 0 {
                // Widening an intermediate entry to user is harmless: the leaf bits decide.
                t.write(table, i, e | PageFlags::USER);
            }
            table = e & ADDR_MASK;
        }
    }
    let i = index(virt, size.level());
    if t.read(table, i) & PageFlags::PRESENT != 0 {
        return Err(MapError::AlreadyMapped);
    }
    t.write(table, i, phys | leaf_bits(flags, size.level()));
    Ok(())
}

/// Translates `virt` (handles 1 GiB and 2 MiB leaves).
pub fn translate(t: &impl Tables, root: u64, virt: u64) -> Option<Leaf> {
    let mut table = root;
    let mut allow = PageFlags::USER | PageFlags::WRITABLE;
    for level in (1..=4).rev() {
        let e = t.read(table, index(virt, level));
        if e & PageFlags::PRESENT == 0 {
            return None;
        }
        if level == 1 || (e & PageFlags::HUGE != 0 && level <= 3) {
            let sz = span(level);
            let fl = leaf_flags(e, level);
            let fl = (fl & !(PageFlags::USER | PageFlags::WRITABLE)) | (fl & allow);
            return Some(Leaf {
                phys: leaf_phys(e, level) + (virt & (sz - 1)),
                page_virt: virt & !(sz - 1),
                size: PageSize::from_level(level),
                flags: PageFlags(fl),
            });
        }
        allow &= e;
        table = e & ADDR_MASK;
    }
    None
}

/// A leaf `unmap` removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unmapped {
    pub virt: u64,
    pub phys: u64,
    pub size: PageSize,
    pub flags: PageFlags,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UnmapStats {
    pub leaves: u64,
    pub tables_freed: u64,
}

/// Removes every leaf in [virt, virt + len) (4 KiB aligned). A large leaf must lie wholly inside the range
/// (`PartialHugePage` otherwise, checked before anything changes). Tables at a level below `free_below`
/// (1 = PT, 2 = PD, 3 = PDPT) that end up empty are freed and their entry cleared; the kernel passes 3 so the
/// PDPTs that every root shares stay. `f` sees each removed leaf; the caller flushes TLBs.
pub fn unmap(t: &mut impl Tables, root: u64, virt: u64, len: u64, free_below: u32, f: &mut dyn FnMut(Unmapped)) -> Result<UnmapStats, MapError> {
    let (lo, hi, high) = unmap_range(virt, len)?;
    if lo == hi {
        return Ok(UnmapStats::default());
    }
    check_partial(t, root, 4, 0, lo, hi)?;
    let mut st = UnmapStats::default();
    walk_unmap(t, root, 4, 0, lo, hi, high, free_below, f, &mut st);
    Ok(st)
}

/// [lo, hi) of an unmap with the sign-extension bits cut off (walks use those) and the bits themselves, to add back
/// for the callback. `lo == hi` for an empty range.
fn unmap_range(virt: u64, len: u64) -> Result<(u64, u64, u64), MapError> {
    if virt & 0xFFF != 0 || len & 0xFFF != 0 {
        return Err(MapError::NotAligned);
    }
    let end = virt.checked_add(len).ok_or(MapError::NotAligned)?;
    if len == 0 {
        return Ok((0, 0, 0));
    }
    // Canonical addresses: walk with the sign-extension bits cut off, add them back for the callback.
    let high = virt & 0xFFFF_0000_0000_0000;
    if (end - 1) & 0xFFFF_0000_0000_0000 != high {
        return Err(MapError::NotAligned);
    }
    Ok((virt & 0xFFFF_FFFF_FFFF, ((end - 1) & 0xFFFF_FFFF_FFFF) + 1, high))
}

/// `unmap` of one range in short steps, so a lock can be let go between them (`kernel::unmap_kernel`): `check` once,
/// `step` until `done`, then `finish`. A step ends at the next 2 MiB boundary, so it clears at most one page table; where
/// the entries say nothing lies there (empty slots, a 1 GiB leaf) it runs on to the end of them, so a leaf comes off in one
/// step and an empty stretch costs one step. Between the steps nothing but the owner of the range may change the range
/// itself.
pub(super) struct UnmapSlices {
    start: u64,
    next: u64,
    hi: u64,
    high: u64,
}

impl UnmapSlices {
    pub(super) fn new(virt: u64, len: u64) -> Result<UnmapSlices, MapError> {
        let (lo, hi, high) = unmap_range(virt, len)?;
        Ok(UnmapSlices { start: lo, next: lo, hi, high })
    }

    pub(super) fn done(&self) -> bool {
        self.next >= self.hi
    }

    /// The range lies inside one 2 MiB, so the first `step` takes all of it.
    pub(super) fn one_step(&self) -> bool {
        self.done() || self.start / span(2) == (self.hi - 1) / span(2)
    }

    /// `PartialHugePage` when a large leaf sticks out of the range, as `check_partial` says for the whole of it, but
    /// in two short walks: a leaf that overlaps the range and sticks out holds its first or its last byte. A range of
    /// 4 KiB leaves would otherwise cost a read per leaf here, in one piece.
    pub(super) fn check(&self, t: &impl Tables, root: u64) -> Result<(), MapError> {
        if self.done() {
            return Ok(());
        }
        for at in [self.start, self.hi - 1] {
            if let Some((s, sz)) = large_leaf_at(t, root, at) {
                if s < self.start || s + sz > self.hi {
                    return Err(MapError::PartialHugePage);
                }
            }
        }
        Ok(())
    }

    /// The next step: frees what it empties as far as `t.may_free` allows.
    pub(super) fn step(&mut self, t: &mut impl Tables, root: u64, free_below: u32, f: &mut dyn FnMut(Unmapped), st: &mut UnmapStats) {
        if self.done() {
            return;
        }
        let end = step_end(t, root, self.next).min(self.hi);
        walk_unmap(t, root, 4, 0, self.next, end, self.high, free_below, f, st);
        self.next = end;
    }

    /// Walks the two ends of the range once more: the tables a `may_free` kept back there (they reach past the range)
    /// are empty by now, and only they can be. Clears nothing, so it costs two paths through the tables.
    pub(super) fn finish(&self, t: &mut impl Tables, root: u64, free_below: u32, st: &mut UnmapStats) {
        if self.start >= self.hi {
            return;
        }
        for at in [self.start, self.hi - 1] {
            walk_unmap(t, root, 4, 0, at, at + 1, self.high, free_below, &mut |_| {}, st);
        }
    }
}

/// Start and size of the large leaf that holds `at`, if one does.
fn large_leaf_at(t: &impl Tables, root: u64, at: u64) -> Option<(u64, u64)> {
    let mut table = root;
    for level in (2..=4).rev() {
        let e = t.read(table, index(at, level));
        if e & PageFlags::PRESENT == 0 {
            return None;
        }
        if e & PageFlags::HUGE != 0 && level <= 3 {
            return Some((at & !(span(level) - 1), span(level)));
        }
        table = e & ADDR_MASK;
    }
    None
}

/// Where the step that starts at `at` ends: see `UnmapSlices`.
fn step_end(t: &impl Tables, root: u64, at: u64) -> u64 {
    let end_of = |level| (at | (span(level) - 1)) + 1;
    let top = t.read(root, index(at, 4));
    if top & PageFlags::PRESENT == 0 {
        return end_of(4);
    }
    let e = t.read(top & ADDR_MASK, index(at, 3));
    if e & PageFlags::PRESENT == 0 || e & PageFlags::HUGE != 0 {
        return end_of(3);
    }
    let pd = e & ADDR_MASK;
    if t.read(pd, index(at, 2)) & PageFlags::PRESENT != 0 {
        return end_of(2);
    }
    let free = (index(at, 2) + 1..512).take_while(|&i| t.read(pd, i) & PageFlags::PRESENT == 0).count() as u64;
    end_of(2) + free * span(2)
}

fn check_partial(t: &impl Tables, table: u64, level: u32, base: u64, lo: u64, hi: u64) -> Result<(), MapError> {
    let sz = span(level);
    let (first, last) = (((lo.max(base) - base) / sz) as usize, ((hi.min(base + 512 * sz) - 1 - base) / sz) as usize);
    for i in first..=last {
        let e = t.read(table, i);
        if e & PageFlags::PRESENT == 0 {
            continue;
        }
        let s = base + i as u64 * sz;
        if level > 1 && e & PageFlags::HUGE != 0 && level <= 3 {
            if s < lo || s + sz > hi {
                return Err(MapError::PartialHugePage);
            }
        } else if level > 1 {
            check_partial(t, e & ADDR_MASK, level - 1, s, lo, hi)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn walk_unmap(t: &mut impl Tables, table: u64, level: u32, base: u64, lo: u64, hi: u64, high: u64, free_below: u32,
    f: &mut dyn FnMut(Unmapped), st: &mut UnmapStats) {
    let sz = span(level);
    let (first, last) = (((lo.max(base) - base) / sz) as usize, ((hi.min(base + 512 * sz) - 1 - base) / sz) as usize);
    for i in first..=last {
        let e = t.read(table, i);
        if e & PageFlags::PRESENT == 0 {
            continue;
        }
        let s = base + i as u64 * sz;
        if level == 1 || (e & PageFlags::HUGE != 0 && level <= 3) {
            t.write(table, i, 0);
            st.leaves += 1;
            f(Unmapped { virt: high | s, phys: leaf_phys(e, level), size: PageSize::from_level(level), flags: PageFlags(leaf_flags(e, level)) });
            continue;
        }
        let child = e & ADDR_MASK;
        walk_unmap(t, child, level - 1, s, lo, hi, high, free_below, f, st);
        if level - 1 < free_below && t.may_free(high | s, sz) && (0..512).all(|j| t.read(child, j) == 0) {
            t.write(table, i, 0);
            t.free_table(child);
            st.tables_freed += 1;
        }
    }
}

/// Rewrites every present leaf in [lo, hi) (one canonical half) whose new flags `f` returns (4 KiB layout;
/// the frame stays); `None` leaves the entry alone. Returns how many entries changed; the caller flushes.
/// A large leaf must lie wholly inside the range (`PartialHugePage` otherwise, checked before anything
/// changes), so a protect never changes rights outside what was asked for.
pub fn update_leaves(t: &mut impl Tables, root: u64, lo: u64, hi: u64, f: &mut dyn FnMut(Unmapped) -> Option<u64>) -> Result<u64, MapError> {
    if hi <= lo || (hi - 1) & 0xFFFF_0000_0000_0000 != lo & 0xFFFF_0000_0000_0000 {
        return Ok(0);
    }
    let high = lo & 0xFFFF_0000_0000_0000;
    #[allow(clippy::too_many_arguments)]
    fn walk(t: &mut impl Tables, table: u64, level: u32, base: u64, lo: u64, hi: u64, high: u64, f: &mut dyn FnMut(Unmapped) -> Option<u64>) -> u64 {
        let sz = span(level);
        let (first, last) = (((lo.max(base) - base) / sz) as usize, ((hi.min(base + 512 * sz) - 1 - base) / sz) as usize);
        let mut n = 0;
        for i in first..=last {
            let e = t.read(table, i);
            if e & PageFlags::PRESENT == 0 {
                continue;
            }
            let s = base + i as u64 * sz;
            if level == 1 || (e & PageFlags::HUGE != 0 && level <= 3) {
                let phys = leaf_phys(e, level);
                let u = Unmapped { virt: high | s, phys, size: PageSize::from_level(level), flags: PageFlags(leaf_flags(e, level)) };
                if let Some(nf) = f(u) {
                    let ne = phys | leaf_bits(nf, level);
                    if ne != e {
                        t.write(table, i, ne);
                        n += 1;
                    }
                }
            } else {
                n += walk(t, e & ADDR_MASK, level - 1, s, lo, hi, high, f);
            }
        }
        n
    }
    let (l, h) = (lo & 0xFFFF_FFFF_FFFF, ((hi - 1) & 0xFFFF_FFFF_FFFF) + 1);
    check_partial(t, root, 4, 0, l, h)?;
    Ok(walk(t, root, 4, 0, l, h, high, f))
}

/// Every present leaf in [lo, hi) of `root` (a read-only walk, e.g. to copy mappings into a new root).
pub fn for_each_leaf(t: &impl Tables, root: u64, lo: u64, hi: u64, f: &mut dyn FnMut(Unmapped)) {
    if hi <= lo || (hi - 1) & 0xFFFF_0000_0000_0000 != lo & 0xFFFF_0000_0000_0000 {
        return;
    }
    let high = lo & 0xFFFF_0000_0000_0000;
    #[allow(clippy::too_many_arguments)]
    fn walk(t: &impl Tables, table: u64, level: u32, base: u64, lo: u64, hi: u64, high: u64, f: &mut dyn FnMut(Unmapped)) {
        let sz = span(level);
        let (first, last) = (((lo.max(base) - base) / sz) as usize, ((hi.min(base + 512 * sz) - 1 - base) / sz) as usize);
        for i in first..=last {
            let e = t.read(table, i);
            if e & PageFlags::PRESENT == 0 {
                continue;
            }
            let s = base + i as u64 * sz;
            if level == 1 || (e & PageFlags::HUGE != 0 && level <= 3) {
                f(Unmapped { virt: high | s, phys: leaf_phys(e, level), size: PageSize::from_level(level), flags: PageFlags(leaf_flags(e, level)) });
            } else {
                walk(t, e & ADDR_MASK, level - 1, s, lo, hi, high, f);
            }
        }
    }
    walk(t, root, 4, 0, lo & 0xFFFF_FFFF_FFFF, ((hi - 1) & 0xFFFF_FFFF_FFFF) + 1, high, f);
}

#[cfg(test)]
#[path = "walk_tests.rs"]
mod tests;
