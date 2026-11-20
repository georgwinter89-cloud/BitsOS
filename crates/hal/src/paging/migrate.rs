//! W35 MM-2: migration entries of user leaves (Linux 7.x include/linux/swapops.h `make_readable_migration_entry`
//! / `make_writable_migration_entry`, mm/rmap.c `try_to_migrate_one`, mm/migrate.c `remove_migration_pte`).
//!
//! A migration takes the present leaf that maps a frame out of the CPU's reach (`freeze_user_leaf`: the entry
//! becomes not present with `MIGRATION` set and everything else kept, then the translation is shot down), copies
//! the frame, and puts the leaf back over the new frame with the rights and Accessed/Dirty bits it had
//! (`thaw_user_leaf`). Linux stores the pfn and the rights in a swap entry; here the entry simply keeps its old bits
//! (the CPU ignores every bit of a not-present entry). MM holds the space's lock from the freeze to the thaw, so
//! the space's own walkers (faults, munmap, exit, fork) never meet a frozen entry; lockless walkers (ksync's futex
//! words, under interrupts off) see a not-present entry and fault, which waits for the lock.

use super::{leaf_ptr, read_entry, table, tlb, walk, MapError, Outcome, PageFlags, PageSize, ADDR_MASK};
use core::sync::atomic::{AtomicU64, Ordering::AcqRel, Ordering::Acquire};

/// Software bit of a not-present user leaf held by a migration (bit 9: ignored by the CPU in every entry, and in a
/// not-present one all bits but PRESENT are).
pub const MIGRATION: u64 = 1 << 9;

/// The entry of the user leaf of `size` at `virt` (4 KiB: in its page table; 2 MiB: in its page directory), present
/// or not; None when the tables above it are missing or `size` is another.
fn slot(root: u64, virt: u64, size: PageSize) -> Option<*mut u64> {
    match size {
        PageSize::Size4K => leaf_ptr(root, virt),
        PageSize::Size2M => {
            let mut t = root;
            for level in [4, 3] {
                let e = read_entry(t, walk::index(virt, level));
                if e & PageFlags::PRESENT == 0 || e & PageFlags::HUGE != 0 {
                    return None;
                }
                t = e & ADDR_MASK;
            }
            // SAFETY: index < 512 within a 4 KiB page directory of `root`'s user half (MM's own tables).
            Some(unsafe { table(t).add(walk::index(virt, 2)) })
        }
        PageSize::Size1G => None,
    }
}

/// The frame bits of a leaf entry of `size` (a 2 MiB entry's PAT bit 12 is not part of its frame).
fn frame_bits(size: PageSize) -> u64 {
    match size {
        PageSize::Size4K => ADDR_MASK,
        _ => ADDR_MASK & !(size.bytes() - 1),
    }
}

fn frame_of(e: u64, size: PageSize) -> u64 {
    e & frame_bits(size)
}

/// Whether `e` (from `slot` of `size`) is a leaf: every page-table entry is; a page-directory entry with HUGE (bit
/// 7 of a 4 KiB entry is its PAT bit instead).
fn leaf_of(e: u64, size: PageSize) -> bool {
    size == PageSize::Size4K || e & PageFlags::HUGE != 0
}

/// The not-present entry a migration leaves for the leaf `e` (everything kept, the CPU ignores it).
fn frozen(e: u64) -> u64 {
    (e & !PageFlags::PRESENT) | MIGRATION
}

/// The present leaf that replaces the frozen entry `e` of `size`, over `phys`.
fn thawed(e: u64, size: PageSize, phys: u64) -> u64 {
    (e & !frame_bits(size) & !MIGRATION) | phys | PageFlags::PRESENT
}

/// The entry of `slot` as an atomic, after the range checks. The reference is used only within the caller's call
/// (the space's lock keeps the table alive; the lifetime says nothing more).
fn checked(root: u64, virt: u64, size: PageSize, phys: u64) -> Result<&'static AtomicU64, MapError> {
    if size == PageSize::Size1G || virt >= crate::USER_TOP || !virt.is_multiple_of(size.bytes()) || phys & !ADDR_MASK != 0 || !phys.is_multiple_of(size.bytes()) {
        return Err(MapError::WrongRange);
    }
    let p = slot(root, virt, size).ok_or(MapError::NotMapped)?;
    // SAFETY: `slot` returns a pointer to an aligned u64 entry inside a live user-half table of `root` (MM's own
    // tables, kept by the caller's space lock). Other CPUs touch it only through their page walkers (atomic A/D
    // updates) and lockless software walkers (atomic loads, `read_entry`), so viewing it as an AtomicU64 is sound.
    Ok(unsafe { AtomicU64::from_ptr(p) })
}

/// Takes the present leaf of `size` at `virt` of `root`'s user half, which maps `expect`, out of reach: it becomes
/// not present with `MIGRATION` set (the frame, rights and Accessed/Dirty bits kept), then the translation is shot
/// down (`tlb::Cause::Migrate`). Returns the entry as it was at the swap (with the bits the hardware set until
/// then) and the shootdown's outcome: copy the frame only when it is confirmed (no CPU can store into it any
/// more), else put the leaf back with `thaw_user_leaf(.., expect)`. `NotMapped` (nothing changed) when no present
/// leaf of that size maps `expect` there. The caller holds the space's lock until the thaw.
pub fn freeze_user_leaf(root: u64, virt: u64, size: PageSize, expect: u64) -> Result<(u64, Outcome), MapError> {
    let a = checked(root, virt, size, expect)?;
    let mut cur = a.load(Acquire);
    loop {
        if cur & PageFlags::PRESENT == 0 || !leaf_of(cur, size) || frame_of(cur, size) != expect {
            return Err(MapError::NotMapped);
        }
        // A compare-exchange: a CPU's walker may set Accessed/Dirty between the load and the store.
        match a.compare_exchange(cur, frozen(cur), AcqRel, Acquire) {
            Ok(_) => break,
            Err(now) => cur = now,
        }
    }
    let o = match size {
        PageSize::Size4K => tlb::flush_user(root, virt, virt + size.bytes(), false, false, tlb::Cause::Migrate),
        _ => tlb::flush_user_huge(root, virt, virt + size.bytes(), tlb::Cause::Migrate),
    };
    Ok((cur, o))
}

/// Puts the frozen leaf of `size` at `virt` (`freeze_user_leaf`) back as a present leaf over `phys` (the new frame,
/// or the old one when the migration gave up), with the rights and Accessed/Dirty bits it had. No shootdown: no
/// CPU caches a not-present entry. `NotMapped` (nothing changed) when no frozen leaf of that size is there.
pub fn thaw_user_leaf(root: u64, virt: u64, size: PageSize, phys: u64) -> Result<(), MapError> {
    let a = checked(root, virt, size, phys)?;
    let cur = a.load(Acquire);
    if cur & (PageFlags::PRESENT | MIGRATION) != MIGRATION || !leaf_of(cur, size) {
        return Err(MapError::NotMapped);
    }
    // The frame bits of the size go, the rest (rights, memory type, A/D, a 2 MiB entry's PAT bit 12) stays.
    let new = thawed(cur, size, phys);
    // Nobody else writes a not-present entry of a locked space (the walkers set A/D only in present ones).
    a.compare_exchange(cur, new, AcqRel, Acquire).map(|_| ()).map_err(|_| MapError::NotMapped)
}

/// Whether the user leaf at `virt` of `root` (4 KiB or 2 MiB) is frozen by a migration now.
pub fn migration_entry(root: u64, virt: u64) -> bool {
    if virt >= crate::USER_TOP {
        return false;
    }
    let base = virt & !(PageSize::Size2M.bytes() - 1);
    if let Some(p) = slot(root, base, PageSize::Size2M) {
        // SAFETY: as in `checked`; an atomic load of a live entry.
        let e = unsafe { AtomicU64::from_ptr(p) }.load(Acquire);
        if e & PageFlags::PRESENT == 0 {
            return e & (MIGRATION | PageFlags::HUGE) == MIGRATION | PageFlags::HUGE;
        }
    }
    slot(root, virt & !0xFFF, PageSize::Size4K).is_some_and(|p| {
        // SAFETY: as above.
        let e = unsafe { AtomicU64::from_ptr(p) }.load(Acquire);
        e & (PageFlags::PRESENT | MIGRATION) == MIGRATION
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frozen_leaf_comes_back_over_the_new_frame_with_its_bits() {
        let (a, d, w, u, nx) = (PageFlags::ACCESSED, PageFlags::DIRTY, PageFlags::WRITABLE, PageFlags::USER, PageFlags::NX);
        // 4 KiB: rights, A/D, NX and the PAT bit (7) stay; only the frame changes.
        let e = 0x1234_5000 | PageFlags::PRESENT | w | u | a | d | nx | PageFlags::PAT_4K;
        let f = frozen(e);
        assert_eq!(f & PageFlags::PRESENT, 0, "not present while frozen");
        assert_eq!(f & MIGRATION, MIGRATION);
        assert_eq!(frame_of(f, PageSize::Size4K), 0x1234_5000, "the old frame is still named");
        let t = thawed(f, PageSize::Size4K, 0x9_8765_4000);
        assert_eq!(t, (e & !0x1234_5000) | 0x9_8765_4000);
        assert!(leaf_of(e, PageSize::Size4K), "a PAT 4 KiB leaf is a leaf");
        // 2 MiB: the PAT bit of a large leaf is bit 12, below the frame; it stays.
        let h = 0x4000_0000 | PageFlags::PRESENT | PageFlags::HUGE | w | u | a | (1 << 12);
        assert_eq!(frame_of(h, PageSize::Size2M), 0x4000_0000);
        let t = thawed(frozen(h), PageSize::Size2M, 0x8_0020_0000);
        assert_eq!(t, (h & !0x4000_0000) | 0x8_0020_0000);
        assert!(!leaf_of(0x5000 | PageFlags::PRESENT, PageSize::Size2M), "a page directory entry naming a table is no leaf");
    }
}
