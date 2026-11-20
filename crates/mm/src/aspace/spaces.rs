//! W35 MM-2: ids of the live address spaces, for the anonymous reverse map. An anonymous frame's owner word
//! (`mm_core` per-frame word, the page cache's (cache id, index) for cache frames) holds (space id, page address)
//! of the space that mapped it last (`AsCore::note_anon`); migration finds the space by its id here and checks the
//! entry under the space's lock (a stale word, a reused id, a frame freed meanwhile: the entry maps another frame,
//! nothing moves). A slot per live space, reused after its drop; no fixed number of spaces (R1): the table grows
//! with fallible reservations, and a space that finds no room gets id 0 (its frames are not migrated).

use super::AsCore;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use ksync::{Spare, SpinLock};

enum Slot<T> {
    Live(Weak<T>),
    /// Free; the next free slot's index plus one (0: none).
    Free(u32),
}

/// The free slots are chained through the table, so `remove` (a space's drop) never needs room; the table grows
/// with its lock dropped (`ksync::Spare`): under the spin lock the heap could neither reclaim nor take single
/// frames, and a failed growth there gave a space id 0 for its life (desk review of MM-2 x FS-W2, #4).
struct Table<T> {
    slots: Vec<Slot<T>>,
    /// The first free slot's index plus one (0: none).
    free: u32,
}

static SPACES: SpinLock<Table<AsCore>> = SpinLock::named("mm.spaces", Table { slots: Vec::new(), free: 0 });

/// Ids fit 28 bits of the owner word (`word`).
const ID_BITS: u32 = 28;
/// The page number of an address below `USER_TOP` (2^47) takes 35 bits.
const PAGE_BITS: u32 = 36;
const MAX: usize = (1 << ID_BITS) - 1;

/// A new id for `me` (0: no memory for the table even with the lock dropped; never migrated).
pub(super) fn add(me: &Weak<AsCore>) -> u32 {
    add_in(&SPACES, me)
}

fn add_in<T>(table: &SpinLock<Table<T>>, me: &Weak<T>) -> u32 {
    let mut spare = Spare::new();
    loop {
        let mut t = table.lock();
        let head = t.free as usize;
        if let Some(&Slot::Free(next)) = head.checked_sub(1).and_then(|i| t.slots.get(i)) {
            t.free = next;
            t.slots[head - 1] = Slot::Live(me.clone());
            return head as u32;
        }
        if t.slots.len() >= MAX {
            return 0;
        }
        if !spare.fits(&mut t.slots, 1) {
            drop(t);
            if spare.grow().is_err() {
                return 0;
            }
            continue;
        }
        t.slots.push(Slot::Live(me.clone()));
        return t.slots.len() as u32;
    }
}

/// The space with id `id` is gone.
pub(super) fn remove(id: u32) {
    remove_in(&SPACES, id)
}

fn remove_in<T>(table: &SpinLock<Table<T>>, id: u32) {
    if id == 0 {
        return;
    }
    let mut t = table.lock();
    let i = (id - 1) as usize;
    if matches!(t.slots.get(i), Some(Slot::Live(_))) {
        let next = t.free;
        let gone = core::mem::replace(&mut t.slots[i], Slot::Free(next));
        t.free = id;
        drop(t);
        drop(gone);
    }
}

/// The live space with id `id`.
pub(crate) fn get(id: u32) -> Option<Arc<AsCore>> {
    if id == 0 {
        return None;
    }
    match SPACES.lock().slots.get((id - 1) as usize)? {
        Slot::Live(w) => w.upgrade(),
        Slot::Free(_) => None,
    }
}

/// Every live space but `except` (the walk for frames several spaces map, MM-2's fallback to Linux's anon_vma):
/// weak references, upgraded by the caller one at a time. The list is made with the table's lock dropped and
/// filled under it (made again if the table grew meanwhile). None: no heap for the list.
pub(crate) fn all_but(except: u32) -> Option<Vec<(u32, Weak<AsCore>)>> {
    all_but_in(&SPACES, except)
}

fn all_but_in<T>(table: &SpinLock<Table<T>>, except: u32) -> Option<Vec<(u32, Weak<T>)>> {
    let mut v: Vec<(u32, Weak<T>)> = Vec::new();
    loop {
        let n = table.lock().slots.len();
        v.try_reserve_exact(n).ok()?;
        let t = table.lock();
        if t.slots.len() > v.capacity() {
            continue;
        }
        for (i, s) in t.slots.iter().enumerate() {
            let id = i as u32 + 1;
            if let Slot::Live(w) = s
                && id != except
                && w.strong_count() > 0
            {
                v.push((id, w.clone()));
            }
        }
        return Some(v);
    }
}

/// The owner word of an anonymous frame mapped at `va` of space `id` (0: none).
pub(crate) fn word(id: u32, va: u64) -> u64 {
    if id == 0 || va >= hal::USER_TOP { 0 } else { (u64::from(id) << PAGE_BITS) | (va >> 12) }
}

/// (space id, page address) of an anonymous frame's owner word.
pub(crate) fn unword(w: u64) -> (u32, u64) {
    ((w >> PAGE_BITS) as u32 & ((1 << ID_BITS) - 1), (w & ((1 << PAGE_BITS) - 1)) << 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_words_round_trip_and_never_name_a_space_without_an_id() {
        for (id, va) in [(1u32, 0u64), (7, hal::USER_TOP - 0x1000), ((1 << ID_BITS) - 1, 0x40_0000), (12345, 0x1000)] {
            let w = word(id, va);
            assert_ne!(w, 0);
            assert_eq!(unword(w), (id, va), "{id} {va:#x}");
        }
        assert_eq!(word(0, 0x1000), 0, "no id: no owner");
        assert_eq!(word(3, hal::USER_TOP), 0, "a kernel address is no program page");
        assert_eq!(unword(0).0, 0, "an unset word names no space");
    }

    /// Freed ids come back newest first and name nothing meanwhile, a twice-freed id is chained once, the table
    /// grows only when no slot is free, and `all_but` lists the live ones but the excepted.
    #[test]
    fn freed_ids_are_chained_and_the_list_is_made_outside() {
        let t = SpinLock::named("mm.test.spaces", Table { slots: Vec::new(), free: 0 });
        let spaces: Vec<Arc<u32>> = (0..40).map(Arc::new).collect();
        let ws: Vec<Weak<u32>> = spaces.iter().map(Arc::downgrade).collect();
        let ids: Vec<u32> = ws.iter().map(|w| add_in(&t, w)).collect();
        assert_eq!(ids, (1..=40).collect::<Vec<u32>>());
        ids[..10].iter().for_each(|&id| remove_in(&t, id));
        remove_in(&t, ids[0]);
        let live: Vec<u32> = all_but_in(&t, 20).expect("list").iter().map(|(i, _)| *i).collect();
        assert_eq!(live, (11..=40).filter(|&i| i != 20).collect::<Vec<u32>>(), "freed and excepted ids are not listed");
        let again: Vec<u32> = ws[..11].iter().map(|w| add_in(&t, w)).collect();
        assert_eq!(&again[..10], &(1..=10).rev().collect::<Vec<u32>>()[..], "newest first, each once");
        assert_eq!(again[10], 41, "a new slot only when none is free");
        assert_eq!(t.lock().slots.len(), 41);
    }
}
