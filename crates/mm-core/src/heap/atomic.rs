//! The detector for large blocks asked for where the heap may not reclaim: a spin lock held, interrupts off, an
//! interrupt handler, a grace section (`reclaim::context_allows` false).
//!
//! Such a request gets what is free now, or the HighAtomic reserve, or null, and null to the Rust allocator's
//! caller is a kernel panic. Three panics on the Mini-PC were of this kind (11.11.2026: blocks of 36 KiB and 17 KiB
//! under spin locks; 12.11.: 17296 bytes cloned under `pci.devices`), each found only when it had happened. So
//! every request for two pages or more made that way is counted (`mm.heap.atomic_large`) and its call chain kept
//! (`callers::HEAP_ATOMIC_LARGE`), whether or not memory was short, and `heap.atomic_large` events name the sites
//! and why the heap could not reclaim there: the call sites to fix are known before one of them fails.
//!
//! Cost: one `context_allows` on the large-block path (a few per-CPU loads, beside a zone lock), nothing on the
//! size classes' path; a request that is caught walks its stack's frame records (cold).

use super::KernelHeap;
use crate::callers::{site_event, HEAP_ATOMIC_LARGE as SITES};

klog::counter!(ATOMIC_LARGE, "mm.heap.atomic_large");
// Requests whose call chain found the table full: counted above, not named by an event.
klog::counter!(UNLISTED, "mm.heap.atomic_large_unlisted");

/// Why the heap may not reclaim for a request, as bits of a site's detail above the size.
const WHY_SHIFT: u32 = 56;
const LOCK: u64 = 1;
const HANDLER: u64 = 2;
const GRACE: u64 = 4;
const IRQS_OFF: u64 = 8;

/// The reasons `reclaim::context_allows` is false here.
fn why() -> u64 {
    let mut w = 0;
    if ksync::preempt_count() != 0 {
        w |= LOCK;
    }
    if hal::percpu::in_hard_irq() {
        w |= HANDLER;
    }
    if crate::grace::in_section() {
        w |= GRACE;
    }
    #[cfg(target_os = "none")]
    if !hal::cpu::interrupts_enabled() {
        w |= IRQS_OFF;
    }
    w
}

/// The boot thread before it turns interrupts on: start-up code, memory is whole, nothing to reclaim yet. (Its
/// stack is the one hal has no bounds for.)
fn booting(why: u64) -> bool {
    #[cfg(target_os = "none")]
    if why == IRQS_OFF && hal::switch::current_stack().top == 0 {
        return true;
    }
    let _ = why;
    false
}

impl KernelHeap {
    /// A block of `size` bytes (two pages or more) is asked for where the heap may not reclaim.
    #[cold]
    #[inline(never)]
    pub(super) fn note_atomic_large(&self, size: usize) {
        let why = why();
        if !self.global || booting(why) {
            return;
        }
        ATOMIC_LARGE.inc();
        SITES.note(size as u64 | why << WHY_SHIFT, size as u64);
    }
}

/// Events for the sites that are new or doubled (`report::flush_reports`: no lock held).
pub(crate) fn flush() {
    UNLISTED.add(SITES.dropped_news());
    SITES.each_news(&mut |s| {
        let why = s.info >> WHY_SHIFT;
        site_event!(Warn, "heap.atomic_large", s, last_size = s.info & ((1 << WHY_SHIFT) - 1), bytes = s.amount, spin_lock = why & LOCK != 0,
            handler = why & HANDLER != 0, grace = why & GRACE != 0, irqs_off = why & IRQS_OFF != 0;
            "a heap block of two pages or more was asked for where the heap may not reclaim (c0..: return addresses, innermost first)");
    });
}
