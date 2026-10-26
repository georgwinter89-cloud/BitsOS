//! Per-CPU magazines of the heap's size classes (Bonwick's magazines in the simple form of Linux's per-CPU
//! page lists): most allocations and frees touch only this CPU's magazine; the cache lock is taken once per
//! `CAP / 2` objects.
//!
//! Correct with many CPUs and interrupts on: a magazine is behind an `IrqSpinLock`, so code that moves to
//! another CPU between reading its CPU number and taking the lock merely uses the other CPU's magazine under
//! its lock (slower, never wrong), and an interrupt can never find a magazine half changed. A CPU without a
//! slot (`PerCpu` sized before that CPU came online, change request MM-1 #3) and the time while the slots
//! are being created use the cache directly.
//!
//! The slots are created on the first heap call: that creation allocates (the `PerCpu` box) and must not
//! find the magazines half made, so it runs while the state is `BUSY`, where every heap call skips the
//! magazines. Capacities are tuning values for how much a CPU keeps cached, not limits (R1).

use crate::slab::geo::NCLASSES;
#[cfg(test)]
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU8, Ordering};
use hal::percpu::PerCpu;
use ksync::IrqSpinLock;

/// Room of the largest magazine.
pub const MAG_MAX: usize = 64;
/// Magazine capacity per class (fewer for big objects, so a CPU keeps at most ~0.3 MiB cached).
pub const CAP: [usize; NCLASSES] = [64, 64, 64, 64, 64, 64, 64, 64, 32, 32, 16];

pub struct Mag {
    n: usize,
    slot: [usize; MAG_MAX],
}

impl Mag {
    const fn new() -> Mag {
        Mag { n: 0, slot: [0; MAG_MAX] }
    }
    pub fn len(&self) -> usize {
        self.n
    }
    pub fn is_empty(&self) -> bool {
        self.n == 0
    }
    pub fn pop(&mut self) -> Option<usize> {
        self.n = self.n.checked_sub(1)?;
        Some(self.slot[self.n])
    }
    /// False when the magazine is full.
    pub fn push(&mut self, v: usize) -> bool {
        if self.n == MAG_MAX {
            return false;
        }
        self.slot[self.n] = v;
        self.n += 1;
        true
    }
    pub fn objects(&self) -> &[usize] {
        &self.slot[..self.n]
    }
    /// Hands the `k` oldest objects (the cold end) to `f`, then drops them from the magazine.
    pub fn drain_oldest(&mut self, k: usize, f: impl FnOnce(&[usize])) {
        let k = k.min(self.n);
        f(&self.slot[..k]);
        self.slot.copy_within(k..self.n, 0);
        self.n -= k;
    }
}

pub type CpuMags = IrqSpinLock<[Mag; NCLASSES]>;

fn new_cpu(_cpu: usize) -> CpuMags {
    IrqSpinLock::named("heap.mags", [const { Mag::new() }; NCLASSES])
}

const UNINIT: u8 = 0;
const BUSY: u8 = 1;
const READY: u8 = 2;

pub enum MagSlots {
    Kernel { per: PerCpu<CpuMags>, state: AtomicU8 },
    /// Host tests: `n` simulated CPUs, `cpu()` says which one the calling thread is on now.
    #[cfg(test)]
    Host { cpus: Vec<CpuMags>, cpu: fn() -> usize },
    /// No magazines (host tests of the plain cache path).
    #[cfg(test)]
    Off,
}

impl MagSlots {
    pub const fn kernel() -> MagSlots {
        MagSlots::Kernel { per: PerCpu::new(new_cpu), state: AtomicU8::new(UNINIT) }
    }

    #[cfg(test)]
    pub fn host(n: usize, cpu: fn() -> usize) -> MagSlots {
        MagSlots::Host { cpus: (0..n).map(new_cpu).collect(), cpu }
    }

    /// The calling CPU's magazines, if it has them. Call holding no heap lock (the first call allocates).
    pub fn this(&self) -> Option<&CpuMags> {
        match self {
            MagSlots::Kernel { per, state } => match state.load(Ordering::Acquire) {
                // Never grows here (an AP's first allocation would re-enter the heap): `prepare_cpus` creates
                // the slots of every possible CPU from the BSP; a CPU without one uses the caches directly.
                READY => per.find_for(hal::percpu::this_cpu()),
                UNINIT if state.compare_exchange(UNINIT, BUSY, Ordering::Acquire, Ordering::Acquire).is_ok() => {
                    // Allocates the slots; the nested heap calls see BUSY and use the caches directly.
                    let _ = per.get_for(0);
                    state.store(READY, Ordering::Release);
                    per.get_for(hal::percpu::this_cpu())
                }
                _ => None,
            },
            #[cfg(test)]
            MagSlots::Host { cpus, cpu } => cpus.get(cpu()),
            #[cfg(test)]
            MagSlots::Off => None,
        }
    }

    /// Creates every possible CPU's magazines (allocates; from the BSP before the APs start, holding no heap
    /// lock). Does nothing before the first heap call made the slots.
    pub fn prepare_cpus(&self) {
        if let MagSlots::Kernel { per, state } = self
            && state.load(Ordering::Acquire) == READY
        {
            per.cover_possible();
        }
    }

    /// Every CPU's magazines that exist (never creates them).
    pub fn for_each(&self, mut f: impl FnMut(&CpuMags)) {
        match self {
            MagSlots::Kernel { per, state } => {
                if state.load(Ordering::Acquire) == READY {
                    per.iter_existing().for_each(&mut f);
                }
            }
            #[cfg(test)]
            MagSlots::Host { cpus, .. } => cpus.iter().for_each(&mut f),
            #[cfg(test)]
            MagSlots::Off => {}
        }
    }
}
