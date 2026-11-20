//! Kernel stacks (SC-1): one guard page plus the stack per slot, slots carved from chunks of vmap space and
//! reused after a thread is gone; frames from mm-core, mapped by `hal::switch::KernelStack`. SC-3: a finished
//! thread's stack comes back (frames and slot) only through the reaper, after every CPU flushed its TLB.
//!
//! Interim (noted for MM-2): MM-2's vmap allocator is not merged, so chunks come from `hal::gdt::cpu_area_alloc`
//! (the top 64 GiB of vmap, CONTRACTS 6a), which never takes a chunk back. Slots inside a chunk are reused.

use alloc::vec::Vec;
use hal::switch::{KernelStack, StackError};
use ksync::{Spare, SpinLock};

/// Unmapped pages below each stack.
pub const GUARD_PAGES: u64 = 1;
/// Pages per stack (64 KiB, as G0).
pub const STACK_PAGES: u64 = 1 << crate::STACK_ORDER;
pub const SLOT_BYTES: u64 = (GUARD_PAGES + STACK_PAGES) * 4096;
/// Slots per chunk of virtual space: a growth step, not a limit (R1); another chunk follows on demand.
pub const SLOTS_PER_CHUNK: u64 = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackStats {
    /// Stacks mapped now.
    pub live: usize,
    /// Slots waiting for reuse.
    pub free_slots: usize,
    /// Chunks of virtual space reserved so far.
    pub chunks: usize,
    /// Slots taken out of use because something was still mapped in them (`stack.slot_in_use`).
    pub quarantined: usize,
}

/// Virtual slots (pure; host-tested). `reserve(len)` hands out a fresh chunk of `len` bytes.
pub struct SlotPool {
    free: Vec<u64>,
    next: u64,
    end: u64,
    chunks: usize,
    live: usize,
    quarantined: usize,
}

impl Default for SlotPool {
    fn default() -> Self {
        Self::new()
    }
}

impl SlotPool {
    pub const fn new() -> Self {
        SlotPool { free: Vec::new(), next: 0, end: 0, chunks: 0, live: 0, quarantined: 0 }
    }

    /// Whether the free list has the room `alloc` wants before it may open a chunk (true also when the next
    /// `alloc` opens none). The list holds every slot of every chunk, so `release` never allocates (it runs
    /// right after a switch, interrupts off); and it gets that room here, from `spare`, which the caller grows
    /// with the pool's lock dropped: a thousand threads' list is two pages, which the heap may not find under
    /// a spin lock.
    pub fn has_room(&mut self, spare: &mut Spare<Vec<u64>>) -> bool {
        let more = self.list_room_wanted();
        spare.fits(&mut self.free, more)
    }

    /// Entries the free list must be able to take for the chunk the next `alloc` would open (0: it opens none).
    fn list_room_wanted(&self) -> usize {
        if !self.free.is_empty() || !self.chunk_used_up() {
            return 0;
        }
        self.chunks.saturating_add(1).saturating_mul(SLOTS_PER_CHUNK as usize)
    }

    fn chunk_used_up(&self) -> bool {
        self.end == 0 || self.next + SLOT_BYTES > self.end
    }

    /// A slot base (its guard page). `Some((base, new_chunk))`; `None` when no chunk is left, or when a chunk
    /// would have to be opened and the free list lacks the room for it (`has_room` first). Never allocates.
    pub fn alloc(&mut self, reserve: &mut dyn FnMut(u64) -> Option<u64>) -> Option<(u64, bool)> {
        if let Some(b) = self.free.pop() {
            self.live += 1;
            return Some((b, false));
        }
        let mut new_chunk = false;
        if self.chunk_used_up() {
            if self.free.capacity() < self.list_room_wanted() {
                return None;
            }
            let len = SLOT_BYTES * SLOTS_PER_CHUNK;
            let base = reserve(len)?;
            self.next = base;
            self.end = base + len;
            self.chunks += 1;
            new_chunk = true;
        }
        let b = self.next;
        self.next += SLOT_BYTES;
        self.live += 1;
        Some((b, new_chunk))
    }

    pub fn release(&mut self, base: u64) {
        self.free.push(base);
        self.live = self.live.saturating_sub(1);
    }

    /// Takes a slot from `alloc` out of use for good (neither live nor free): it is never handed out again.
    pub fn quarantine(&mut self) {
        self.live = self.live.saturating_sub(1);
        self.quarantined += 1;
    }

    pub fn stats(&self) -> StackStats {
        StackStats { live: self.live, free_slots: self.free.len(), chunks: self.chunks, quarantined: self.quarantined }
    }
}

static POOL: SpinLock<SlotPool> = SpinLock::named("sched.stacks", SlotPool::new());

klog::counter!(CHUNKS, "sched.stack_chunks");

fn free_frame(p: u64) {
    if let Some(fa) = mm_core::frames() {
        fa.free(p, 0, mm_core::Kind::Stack);
    }
}

fn release_slot(base: u64) {
    POOL.lock().release(base);
}

/// How long a spawn that found no memory waits for dead stacks to come back before it fails: two reaper passes
/// at their slowest (a pass waits hal's give-up time for a CPU that does not answer its shootdown).
const NO_MEMORY_WAIT_NS: u64 = 2 * hal::paging::tlb::GIVE_UP_NS;

/// A fresh kernel stack with its guard page. Dead stacks come first: with many of them waiting the caller lets
/// the reaper catch up (`reap::relieve_backlog`), and a failure for want of memory is tried again while dead
/// stacks were waiting and the reaper frees some (bounded), so a spawn fails only when the memory is in use.
pub fn alloc() -> Result<KernelStack, StackError> {
    crate::reap::relieve_backlog();
    let t0 = time::now_ns();
    loop {
        // Counted before the try: a try that fails part-way retires its own pages, which is no memory to wait for.
        let dead = hal::switch::retired_count();
        let r = alloc_once();
        let short = matches!(r, Err(StackError::NoMemory | StackError::Retired(None)));
        let left = NO_MEMORY_WAIT_NS.saturating_sub(time::now_ns().saturating_sub(t0));
        if !short || dead == 0 || left == 0 || !crate::reap::await_freed(left) {
            return r;
        }
    }
}

/// The pool lock is held while the stack is mapped, so a slot and its pages appear together. (A freed stack is
/// unmapped by the reaper without it: clearing its own entries creates no table.)
fn alloc_once() -> Result<KernelStack, StackError> {
    let fa = mm_core::frames().ok_or(StackError::NoMemory)?;
    // Before the guard: the free list it replaces is freed after the unlock.
    let mut spare = Spare::new();
    let (r, base, new_chunk) = {
        let mut pool = loop {
            let mut pool = POOL.lock();
            if pool.has_room(&mut spare) {
                break pool;
            }
            // Nothing was taken yet: another spawn may open the chunk meanwhile, then the look above asks for
            // the next chunk's room or for none.
            drop(pool);
            spare.grow().map_err(|_| StackError::NoMemory)?;
        };
        let (base, new_chunk) = pool.alloc(&mut hal::gdt::cpu_area_alloc).ok_or(StackError::NoMemory)?;
        let r = KernelStack::map(
            base,
            GUARD_PAGES,
            STACK_PAGES,
            &mut || fa.alloc(0, mm_core::Zone::Normal, mm_core::Kind::Stack),
            &mut mm_core::alloc_table_frame,
            free_frame,
            release_slot,
        );
        match r {
            // hal retired what it had mapped and gives the slot back through `release_slot` once reaped.
            Err(StackError::Retired(_)) | Ok(_) => {}
            // Something is still mapped in the slot: back on the free list it would be handed out first again
            // and every later spawn would fail on it, so it leaves the pool.
            Err(StackError::InUse) => pool.quarantine(),
            Err(_) => pool.release(base),
        }
        (r, base, new_chunk)
    };
    if let Err(StackError::InUse) = r {
        let root = hal::switch::kernel_root();
        let mapped = (0..GUARD_PAGES + STACK_PAGES).filter(|i| hal::paging::translate(root, base + i * 4096).is_some()).count();
        klog::event!(Error, "sched", "stack.slot_in_use", base = klog::Hex(base), mapped = mapped, root = klog::Hex(root),
            retired = hal::switch::retired_count(); "a free stack slot still has pages mapped: slot taken out of the pool");
    }
    if let Err(StackError::Retired(e)) = r {
        klog::event!(Warn, "sched", "stack.map_failed", base = klog::Hex(base), map_error = e.is_some(); "partly mapped stack retired");
        crate::reap::kick();
    }
    if new_chunk {
        CHUNKS.inc();
        klog::event!(Info, "sched", "stack.chunk", base = klog::Hex(base), slots = SLOTS_PER_CHUNK, slot_kib = SLOT_BYTES / 1024);
    }
    r
}

pub fn stats() -> StackStats {
    POOL.lock().stats()
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    /// `alloc` as `alloc_once` calls it: the free list's room first.
    fn take(p: &mut SlotPool, reserve: &mut dyn FnMut(u64) -> Option<u64>) -> Option<(u64, bool)> {
        let mut spare = Spare::new();
        while !p.has_room(&mut spare) {
            spare.grow().unwrap();
        }
        p.alloc(reserve)
    }

    #[test]
    fn slots_are_reused_and_chunks_grow_without_limit() {
        let mut p = SlotPool::new();
        let mut next_chunk = 0x1000_0000u64;
        let mut reserve = |len: u64| {
            let b = next_chunk;
            next_chunk += len + 0x10_0000;
            Some(b)
        };
        let n = (SLOTS_PER_CHUNK * 3 + 5) as usize;
        let mut got = std::vec::Vec::new();
        for _ in 0..n {
            got.push(take(&mut p, &mut reserve).unwrap().0);
        }
        assert_eq!(p.stats(), StackStats { live: n, free_slots: 0, chunks: 4, quarantined: 0 });
        let mut sorted = got.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "slots overlap");
        assert!(got.windows(2).all(|w| w[1] >= w[0] + SLOT_BYTES));
        for b in got.drain(..10) {
            p.release(b);
        }
        assert_eq!(p.stats().free_slots, 10);
        let (b, fresh) = take(&mut p, &mut reserve).unwrap();
        assert!(!fresh && sorted.contains(&b), "a released slot is reused first");
        assert_eq!(p.stats().chunks, 4);
        assert!(p.free.capacity() >= 4 * SLOTS_PER_CHUNK as usize);
    }

    /// The list's room comes from the spare only: `alloc` alone never grows it (it runs under the pool's spin
    /// lock), and with the room made every slot of every chunk can come back without an allocation.
    #[test]
    fn the_free_list_grows_only_through_the_spare() {
        let mut p = SlotPool::new();
        let mut next_chunk = 0x1000_0000u64;
        let mut reserve = |len: u64| {
            let b = next_chunk;
            next_chunk += len;
            Some(b)
        };
        assert!(p.alloc(&mut reserve).is_none(), "a chunk was opened without room in the free list");
        assert_eq!(p.stats().chunks, 0);
        let mut spare = Spare::new();
        assert!(!p.has_room(&mut spare));
        spare.grow().unwrap();
        assert!(p.has_room(&mut spare));
        let mut got = std::vec::Vec::new();
        for i in 0..2 * SLOTS_PER_CHUNK {
            let mut spare = Spare::new();
            let mut grown = 0;
            while !p.has_room(&mut spare) {
                spare.grow().unwrap();
                grown += 1;
            }
            assert_eq!(grown, usize::from(i == SLOTS_PER_CHUNK), "slot {i}: the list grows once per chunk");
            let cap = p.free.capacity();
            got.push(p.alloc(&mut reserve).unwrap().0);
            assert_eq!(p.free.capacity(), cap, "alloc allocated");
        }
        let cap = p.free.capacity();
        assert!(cap >= got.len());
        for b in got {
            p.release(b);
        }
        assert_eq!(p.free.capacity(), cap, "release allocated");
        // A spawn that lost the race for the chunk: its spare is not needed and nothing is asked of it.
        let mut late = Spare::new();
        assert!(p.has_room(&mut late));
    }

    #[test]
    fn a_quarantined_slot_is_neither_live_nor_reused() {
        let mut p = SlotPool::new();
        let mut reserve = |_| Some(0x1000_0000u64);
        let (a, _) = take(&mut p, &mut reserve).unwrap();
        let (b, _) = take(&mut p, &mut reserve).unwrap();
        p.quarantine();
        p.release(b);
        assert_eq!(p.stats(), StackStats { live: 0, free_slots: 1, chunks: 1, quarantined: 1 });
        assert_eq!(take(&mut p, &mut reserve).unwrap().0, b);
        assert_ne!(take(&mut p, &mut reserve).unwrap().0, a, "a quarantined slot came back");
    }

    #[test]
    fn no_virtual_space_is_an_error_not_a_panic() {
        let mut p = SlotPool::new();
        assert!(take(&mut p, &mut |_| None).is_none());
        assert_eq!(p.stats().live, 0);
    }
}
