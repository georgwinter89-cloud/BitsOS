//! Kernel stacks (SC-1): one guard page plus the stack per slot, slots carved from chunks of vmap space and
//! reused after a thread is gone; frames from mm-core, mapped by `hal::switch::KernelStack`. SC-3: a finished
//! thread's stack comes back (frames and slot) only through the reaper, after every CPU flushed its TLB.
//!
//! Interim (noted for MM-2): MM-2's vmap allocator is not merged, so chunks come from `hal::gdt::cpu_area_alloc`
//! (the top 64 GiB of vmap, CONTRACTS 6a), which never takes a chunk back. Slots inside a chunk are reused.

use alloc::vec::Vec;
use hal::switch::{KernelStack, StackError};
use ksync::SpinLock;

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

    /// A slot base (its guard page). `Some((base, new_chunk))`; `None` when no chunk or no memory for the
    /// free list is left.
    pub fn alloc(&mut self, reserve: &mut dyn FnMut(u64) -> Option<u64>) -> Option<(u64, bool)> {
        if let Some(b) = self.free.pop() {
            self.live += 1;
            return Some((b, false));
        }
        let mut new_chunk = false;
        if self.end == 0 || self.next + SLOT_BYTES > self.end {
            // Room in the free list for every slot of the chunk now, so `release` never allocates (it runs
            // right after a switch, interrupts off).
            self.free.try_reserve(self.chunks.saturating_add(1).saturating_mul(SLOTS_PER_CHUNK as usize).saturating_sub(self.free.len())).ok()?;
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

/// A fresh kernel stack with its guard page. The pool lock is held while the stack is mapped: `map_4k` does not
/// serialise page-table creation, and two CPUs mapping stacks into the same new page table would lose one.
/// (A freed stack is unmapped by the reaper without it: clearing its own entries creates no table.)
pub fn alloc() -> Result<KernelStack, StackError> {
    let fa = mm_core::frames().ok_or(StackError::NoMemory)?;
    let (r, base, new_chunk) = {
        let mut pool = POOL.lock();
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
            got.push(p.alloc(&mut reserve).unwrap().0);
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
        let (b, fresh) = p.alloc(&mut reserve).unwrap();
        assert!(!fresh && sorted.contains(&b), "a released slot is reused first");
        assert_eq!(p.stats().chunks, 4);
        assert!(p.free.capacity() >= 4 * SLOTS_PER_CHUNK as usize);
    }

    #[test]
    fn a_quarantined_slot_is_neither_live_nor_reused() {
        let mut p = SlotPool::new();
        let mut reserve = |_| Some(0x1000_0000u64);
        let (a, _) = p.alloc(&mut reserve).unwrap();
        let (b, _) = p.alloc(&mut reserve).unwrap();
        p.quarantine();
        p.release(b);
        assert_eq!(p.stats(), StackStats { live: 0, free_slots: 1, chunks: 1, quarantined: 1 });
        assert_eq!(p.alloc(&mut reserve).unwrap().0, b);
        assert_ne!(p.alloc(&mut reserve).unwrap().0, a, "a quarantined slot came back");
    }

    #[test]
    fn no_virtual_space_is_an_error_not_a_panic() {
        let mut p = SlotPool::new();
        assert!(p.alloc(&mut |_| None).is_none());
        assert_eq!(p.stats().live, 0);
    }
}
