//! AB-15: the call and error counters of the syscall table (B3 statistics), one array per CPU. The former
//! per-number counters sat in one shared table, so every syscall of a multi-threaded program bounced the
//! same cache lines between its CPUs; here a call touches only lines of its own CPU (padded by a line on
//! each side, so no other allocation shares them). Readers sum over the CPUs.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, Ordering};
use hal::percpu::PerCpu;

/// Unused counters before and after each CPU's array: one cache line.
const PAD: usize = 8;

static COUNTS: PerCpu<Box<[AtomicU64]>> = PerCpu::new(make);

fn make(_cpu: usize) -> Box<[AtomicU64]> {
    array(crate::table().len())
}

/// One CPU's array for a table of `numbers` entries: a call and an error counter per number, plus the pads.
fn array(numbers: usize) -> Box<[AtomicU64]> {
    (0..2 * numbers + 2 * PAD).map(|_| AtomicU64::new(0)).collect()
}

/// One call of the registered number `nr`.
#[inline]
pub(crate) fn call(nr: usize) {
    bump(COUNTS.get(), 2 * nr);
}

/// One error answer (-4095..0) of `nr`.
#[inline]
pub(crate) fn error(nr: usize) {
    bump(COUNTS.get(), 2 * nr + 1);
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
    (0..hal::percpu::possible_cpus()).filter_map(|c| COUNTS.find_for(c)).map(|a| &a[..])
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
}
