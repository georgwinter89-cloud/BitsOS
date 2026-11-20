//! AB-15: the call and error counters of the syscall table (B3 statistics), one array per CPU. The former
//! per-number counters sat in one shared table, so every syscall of a multi-threaded program bounced the
//! same cache lines between its CPUs; here a call touches only lines of its own CPU (padded by a line on
//! each side, so no other allocation shares them). Readers sum over the CPUs.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::percpu::PerCpu;
use ksync::{OnceRef, SpinLock};

/// Unused counters before and after each CPU's array: one cache line.
const PAD: usize = 8;

/// Each CPU's array, once it is made. The container holds only the reference: `PerCpu` makes its values with
/// interrupts off, where the heap may not reclaim, and an array is two pages.
static COUNTS: PerCpu<OnceRef<[AtomicU64]>> = PerCpu::new(empty);
/// Decides whose array is published when two threads make one for the same CPU; nothing else is done under it.
static INSTALL: SpinLock<()> = SpinLock::named("syscall.counts", ());
klog::counter!(LATE, "syscall.counts_late");

fn empty(_cpu: usize) -> OnceRef<[AtomicU64]> {
    OnceRef::new()
}

/// Kernel init (thread context): the arrays of every CPU the firmware lists, so no syscall makes one.
pub(crate) fn init() {
    for c in 0..hal::percpu::possible_cpus() {
        if let Some(slot) = COUNTS.get_for(c) {
            install(slot, crate::table().len());
        }
    }
}

/// The array of `slot`'s CPU, made here if it has none (init, or the first syscall on a CPU beyond the
/// firmware's list). The array is allocated with no lock held; a caller that lost the race frees its own.
#[cold]
fn install(slot: &'static OnceRef<[AtomicU64]>, numbers: usize) -> &'static [AtomicU64] {
    if let Some(a) = slot.get() {
        return a;
    }
    let fresh = array(numbers);
    let g = INSTALL.lock();
    if let Some(a) = slot.get() {
        drop(g);
        drop(fresh);
        return a;
    }
    let a: &'static [AtomicU64] = Box::leak(fresh);
    slot.set(a);
    a
}

/// This CPU's array.
#[inline]
fn mine() -> &'static [AtomicU64] {
    let slot = COUNTS.get();
    match slot.get() {
        Some(a) => a,
        None => late(slot),
    }
}

/// A syscall found its CPU without an array. Counted when the firmware lists that CPU: `init` should have
/// made it, and here the two pages are asked for on the syscall path (`ab.syscall.counts_at_init`).
#[cold]
fn late(slot: &'static OnceRef<[AtomicU64]>) -> &'static [AtomicU64] {
    if listed(slot) {
        LATE.inc();
    }
    install(slot, crate::table().len())
}

/// Whether `slot` belongs to a CPU the firmware lists. By the slot, not by the CPU number: the thread may
/// have moved to another CPU since it read the slot.
fn listed(slot: &'static OnceRef<[AtomicU64]>) -> bool {
    (0..hal::percpu::possible_cpus()).filter_map(|c| COUNTS.find_for(c)).any(|s| core::ptr::eq(s, slot))
}

/// Arrays made by a syscall for a CPU the firmware lists (0 when `init` covered them all).
pub(crate) fn late_installs() -> u64 {
    LATE.get()
}

/// One CPU's array for a table of `numbers` entries: a call and an error counter per number, plus the pads.
fn array(numbers: usize) -> Box<[AtomicU64]> {
    (0..2 * numbers + 2 * PAD).map(|_| AtomicU64::new(0)).collect()
}

/// One call of the registered number `nr`.
#[inline]
pub(crate) fn call(nr: usize) {
    bump(mine(), 2 * nr);
}

/// One error answer (-4095..0) of `nr`.
#[inline]
pub(crate) fn error(nr: usize) {
    bump(mine(), 2 * nr + 1);
}

/// Counter `i` of one CPU's array; an index past the table touches nothing.
#[inline]
fn bump(a: &[AtomicU64], i: usize) {
    // Atomic although the line is this CPU's: the thread may move to another CPU between `get` and the add.
    if let Some(c) = slot(a, i) {
        c.fetch_add(1, Ordering::Relaxed);
    }
}

/// Counter `i` without the pads (None past the table, so the pads never count).
#[inline]
fn slot(a: &[AtomicU64], i: usize) -> Option<&AtomicU64> {
    a.get(PAD..a.len().saturating_sub(PAD))?.get(i)
}

pub(crate) fn calls(nr: usize) -> u64 {
    sum(cpu_arrays(), 2 * nr)
}

pub(crate) fn errors(nr: usize) -> u64 {
    sum(cpu_arrays(), 2 * nr + 1)
}

/// Never allocates: CPUs that never counted have no array.
fn cpu_arrays() -> impl Iterator<Item = &'static [AtomicU64]> {
    (0..hal::percpu::possible_cpus()).filter_map(|c| COUNTS.find_for(c)).filter_map(|s| s.get())
}

/// CPUs that have their array (ktests).
pub(crate) fn cpus_with_arrays() -> usize {
    cpu_arrays().count()
}

fn sum<'a>(arrays: impl Iterator<Item = &'a [AtomicU64]>, i: usize) -> u64 {
    arrays.filter_map(|a| slot(a, i)).map(|c| c.load(Ordering::Relaxed)).sum()
}

/// CPUs whose array holds a call of `nr` (ktests: proves the calls were spread before checking the sum).
pub(crate) fn cpus_with_calls(nr: usize) -> usize {
    cpu_arrays().filter(|a| slot(a, 2 * nr).is_some_and(|c| c.load(Ordering::Relaxed) > 0)).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_over_cpus_and_ignores_numbers_past_the_table() {
        // Three fake CPUs over a table of 4 numbers; the third never counted.
        let cpus = [array(4), array(4), array(4)];
        bump(&cpus[0], 2 * 3);
        bump(&cpus[1], 2 * 3);
        bump(&cpus[1], 2 * 3 + 1);
        bump(&cpus[0], 2 * 0);
        let all = || cpus.iter().map(|a| &a[..]);
        assert_eq!(sum(all(), 2 * 3), 2);
        assert_eq!(sum(all(), 2 * 3 + 1), 1);
        assert_eq!(sum(all(), 2 * 0), 1);
        assert_eq!(sum(all(), 2 * 2), 0);
        // Past the table (in the trailing pad, past the array, the largest index): no count, no panic.
        for i in [2 * 4, 2 * 4 + 1, 2 * (4 + PAD), usize::MAX] {
            bump(&cpus[2], i);
            assert_eq!(sum(all(), i), 0, "index {i} past the table counted");
        }
        assert!(cpus[2].iter().all(|c| c.load(Ordering::Relaxed) == 0), "a pad of CPU 2 counted");
    }

    /// Several threads find a CPU without its array at once (each makes one, with no lock held): all count
    /// into the same array, the others are freed, and no call is lost.
    #[test]
    fn racing_first_calls_share_one_array() {
        extern crate std;
        for _ in 0..200 {
            let slot: &'static OnceRef<[AtomicU64]> = Box::leak(Box::new(OnceRef::new()));
            let hs: std::vec::Vec<_> = (0..8)
                .map(|_| {
                    std::thread::spawn(move || {
                        let a = install(slot, 4);
                        bump(a, 2 * 1);
                        a.as_ptr() as usize
                    })
                })
                .collect();
            let got: std::vec::Vec<usize> = hs.into_iter().map(|h| h.join().unwrap()).collect();
            let a = slot.get().unwrap();
            assert!(got.iter().all(|&p| p == a.as_ptr() as usize), "two arrays were published for one CPU");
            assert_eq!(sum(core::iter::once(a), 2 * 1), 8);
            assert_eq!(a.len(), 2 * 4 + 2 * PAD);
        }
    }
}
