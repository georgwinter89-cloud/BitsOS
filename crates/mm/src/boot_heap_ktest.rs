//! MM2-NIGHT: the boot asks for no heap block of two pages or more where the heap may not reclaim
//! (`mm.heap.atomic_large`, ATOMIC-3's detector). Its two chains of every boot were the heap's per-CPU magazines,
//! made at the first heap call under lockdep's spin lock (5752 bytes), and the kept-block list of the scattered
//! blocks, reserved under `mm.scatter.kept` (6144 bytes); both are made outside any lock now (`MagSlots::make`
//! from the heap's seed, `scatter::install`). The count is taken at a late initcall, before any ktest adds chains.

use core::sync::atomic::{AtomicU64, Ordering::Relaxed};
use ktest::{kassert, ktest, TestResult};

const UNSET: u64 = u64::MAX;
static AT_BOOT: AtomicU64 = AtomicU64::new(UNSET);

fn metric(name: &str) -> u64 {
    klog::metrics::snapshot().find(|m| m.0 == name).map_or(0, |m| m.1)
}

fn mark() -> Result<(), &'static str> {
    AT_BOOT.store(metric("mm.heap.atomic_large"), Relaxed);
    Ok(())
}
kreg::initcall!(kreg::Level::Late, "mm.heap.boot_mark", mark);

fn boot_clean() -> TestResult {
    let n = AT_BOOT.load(Relaxed);
    kassert!(n != UNSET, "the late initcall mm.heap.boot_mark did not run");
    kassert!(n == 0, "{n} heap blocks of two pages or more were asked for where the heap may not reclaim during the boot (heap.atomic_large events name them)");
    Ok(())
}
// QemuOnly: the Mini-PC's drivers have boot paths of their own; QEMU's boot is the one this checks.
ktest!("mm.heap.boot_atomic_clean", QemuOnly, boot_clean);
