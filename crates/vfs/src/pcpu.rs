//! BL-0: counters for the read path, counted per CPU (Linux `percpu_counter`, lib/percpu_counter.c
//! `percpu_counter_add_batch`): each CPU adds into its own cache-line slot and folds the slot into the shared
//! `klog::counter!` only once it reaches `batch`, so a hot path writes no line another CPU writes and the
//! `metrics` tool still lists the counter by name. The shared value lags the truth by less than `batch` per
//! CPU (Linux's `percpu_counter_read` lags the same way); `sum` is exact (ktests, host tests).
//!
//! The slots are made once for every possible CPU by `prepare_all` (an initcall, where allocating is fine;
//! never on the counting path). A CPU without a slot yet counts straight into the shared counter, so nothing
//! is ever lost.

use core::sync::atomic::{AtomicU64, Ordering};
use hal::percpu::{this_cpu, PerCpu};

/// One CPU's part of a counter, alone on its cache line.
#[repr(align(64))]
pub struct Slot(AtomicU64);

fn new_slot(_cpu: usize) -> Slot {
    Slot(AtomicU64::new(0))
}

pub struct PcpuCounter {
    total: &'static klog::metrics::Counter,
    slots: PerCpu<Slot>,
    batch: u64,
}

impl PcpuCounter {
    pub const fn new(total: &'static klog::metrics::Counter, batch: u64) -> Self {
        PcpuCounter { total, slots: PerCpu::new(new_slot), batch }
    }

    /// The counter's name in `metrics`.
    pub fn name(&self) -> &'static str {
        self.total.name
    }

    #[inline]
    pub fn add(&self, n: u64) {
        match self.slots.find_for(this_cpu()) {
            Some(s) => {
                // A thread that moved to another CPU meanwhile adds into this slot: still counted once.
                if s.0.fetch_add(n, Ordering::Relaxed).wrapping_add(n) >= self.batch {
                    self.total.add(s.0.swap(0, Ordering::Relaxed));
                }
            }
            None => self.total.add(n),
        }
    }

    #[inline]
    pub fn inc(&self) {
        self.add(1)
    }

    /// The exact value: the shared part plus every CPU's slot.
    pub fn sum(&self) -> u64 {
        self.slots.iter_existing().fold(self.total.get(), |a, s| a.wrapping_add(s.0.load(Ordering::Relaxed)))
    }

    /// Makes the slots of every possible CPU (allocates).
    pub fn prepare(&self) {
        self.slots.cover_possible();
    }
}

/// A registered per-CPU counter (`pcpu_counter!`).
pub struct PcpuRef(pub &'static PcpuCounter);
kreg::declare!(pub PCPU_COUNTERS: PcpuRef = "pcpucounter");

/// Makes every registered counter's slots (the initcall; host tests call it to count per CPU).
pub fn prepare_all() -> Result<(), &'static str> {
    for c in PCPU_COUNTERS.entries() {
        c.0.prepare();
    }
    Ok(())
}
kreg::initcall!(kreg::Level::Subsys, "vfs.pcpu_counters", prepare_all);

/// `vfs::pcpu_counter!(NAME, "subsystem.metric", batch);` declares a per-CPU counter shown in `metrics` as
/// "subsystem.metric". `batch`: how much a CPU gathers before it folds into the shared value (bytes: 256 KiB
/// is a good size; events: 64).
#[macro_export]
macro_rules! pcpu_counter {
    ($vis:vis $id:ident, $name:literal, $batch:expr) => {
        $vis static $id: $crate::pcpu::PcpuCounter = {
            $crate::__klog::counter!(TOTAL, $name);
            $crate::pcpu::PcpuCounter::new(&TOTAL, $batch)
        };
        $crate::__kreg::register!("pcpucounter", $crate::pcpu::PcpuRef, $crate::pcpu::PcpuRef(&$id));
    };
}

/// Batch for byte counters: 256 KiB per CPU.
pub const BYTES_BATCH: u64 = 256 << 10;
/// Batch for event counters.
pub const EVENTS_BATCH: u64 = 64;
