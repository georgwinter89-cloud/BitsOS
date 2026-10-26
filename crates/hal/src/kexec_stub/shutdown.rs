//! RB-quiesce: the other CPUs parked before a reboot's or power-off's quiesce, with Reload's stop (`park`,
//! step 1 only). Linux does the same in `native_machine_shutdown` (linux-6.6/arch/x86/kernel/reboot.c:717,
//! `stop_other_cpus` at :742; smp.c:149 `native_stop_other_cpus`: REBOOT_VECTOR first, NMI after a timeout), so a
//! driver's Quiesce undo (SN-5 DP audio, GX-3 scanout restore, WL-4 chip off) never meets its own work running on
//! another CPU.
//!
//! Unlike Reload, the shutdown keeps interrupts on and every CPU's LAPIC as it is (no `Stopped::quiet`): the reset
//! and power-off sequences wait with the scheduler's sleep on CPU 0, and a power-off that did not happen must be
//! able to give the CPUs back (dropping `ShutdownPark`). A refusal (another stop runs, a CPU answered neither the
//! IPI nor the NMI, or one answered only the NMI) releases whatever parked and the shutdown goes on beside the
//! running CPUs, as it did before this item: a reset that happens is worth more than a refused one.
//!
//! Locks after a park: every parked CPU took the park IPI with interrupts on and passed the park guard (SC:
//! preemption count 0), so it holds no `SpinLock` or `IrqSpinLock`; the heap, klog's ring and the Quiesce list
//! stay usable on CPU 0. A CPU parked by NMI may hold any of them, which is why that outcome is refused
//! (`lock_safe`, the same verdict as Reload's).

use super::park::{self, StopError, StopOptions, StopReport, Stopped};
use alloc::format;
use alloc::string::String;

/// How long the other CPUs get to stop, for Reload and shutdown alike: the park IPI, then one NMI. Generous
/// under a hypervisor (timing is never judged there); an AP with interrupts on answers in microseconds.
pub fn stop_options(clock: fn() -> u64, hypervisor: bool) -> StopOptions {
    let s = if hypervisor { 5_000_000_000 } else { 1_000_000_000 };
    StopOptions { clock, irq_wait_ns: s, nmi_wait_ns: Some(s) }
}

/// The options for this machine (`stop_options` with the CPUID hypervisor bit).
pub fn live_stop_options(clock: fn() -> u64) -> StopOptions {
    stop_options(clock, crate::cpu::detect().hypervisor)
}

/// Whether CPU 0 may take locks beside the parked CPUs (pure): false when one was parked by NMI, i.e. it had
/// interrupts off for the whole IPI wait (a kernel bug, e.g. spinning on an `IrqSpinLock`) and may hold any lock.
pub fn lock_safe(r: &StopReport) -> bool {
    r.by_nmi == 0
}

/// Why a shutdown went on without parking the other CPUs (they run).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParkRefusal {
    /// The thread could not move to CPU 0 (only the boot CPU stops the others).
    NotOnBootCpu,
    Stop(StopError),
    /// Parked, but some only by NMI (see `lock_safe`): released again.
    ByNmi { cpus: usize, first: Option<usize> },
}

impl ParkRefusal {
    /// One line for the log (pure).
    pub fn describe(&self) -> String {
        match self {
            ParkRefusal::NotOnBootCpu => String::from("could not move to CPU 0"),
            ParkRefusal::Stop(StopError::NotStopped { cpus, nmi_sent }) => {
                format!("CPU(s) {cpus:?} answered neither the park IPI nor the NMI ({nmi_sent} NMI sent)")
            }
            ParkRefusal::Stop(e) => format!("{e:?}"),
            ParkRefusal::ByNmi { cpus, first } => format!(
                "{cpus} CPU(s) (first: CPU {}) had interrupts off and answered only an NMI: released, they may hold a lock the quiesce needs",
                first.map_or(String::from("?"), |c| format!("{c}"))
            ),
        }
    }
}

/// The result of `park_for_shutdown`. While it lives (and parked), the other CPUs wait in the park loop and the
/// calling thread stays pinned to CPU 0; dropping it releases them and unpins the thread. A reboot never drops it.
pub struct ShutdownPark {
    stopped: Option<Stopped>,
    report: StopReport,
    refusal: Option<ParkRefusal>,
    guard_deferred: u64,
    stray_ipis: u64,
    pin: Option<fn(Option<usize>) -> bool>,
}

impl ShutdownPark {
    /// True when every other CPU is parked (also with only one CPU: nothing to park).
    pub fn parked(&self) -> bool {
        self.refusal.is_none()
    }

    /// What the stop did (`others` = the other online CPUs; zero counts on a refusal before the stop).
    pub fn report(&self) -> StopReport {
        self.report
    }

    pub fn refusal(&self) -> Option<&ParkRefusal> {
        self.refusal.as_ref()
    }

    /// Park IPIs the park guard deferred during this stop (an AP was in a spin-locked section), and park IPIs
    /// that arrived outside a stop meanwhile.
    pub fn guard_deferred(&self) -> u64 {
        self.guard_deferred
    }

    pub fn stray_ipis(&self) -> u64 {
        self.stray_ipis
    }
}

impl Drop for ShutdownPark {
    fn drop(&mut self) {
        // The CPUs first, then the pin (moving the thread needs nothing from them, but the order keeps "pinned
        // while parked" true).
        drop(self.stopped.take());
        if let Some(pin) = self.pin.take() {
            pin(None);
        }
    }
}

/// Moves the calling thread to CPU 0 (`pin`, the scheduler's `pin_current`) and parks every other online CPU
/// (`park::stop_others`, step 1 of Reload's stop), or refuses and leaves them running. Thread context; allocates
/// only before the first park IPI (and on a refusal, after the release). Callers that run unpinned: the drop
/// unpins with `pin(None)`.
pub fn park_for_shutdown(o: &StopOptions, pin: fn(Option<usize>) -> bool) -> ShutdownPark {
    let mut p = ShutdownPark { stopped: None, report: StopReport::default(), refusal: None, guard_deferred: 0, stray_ipis: 0, pin: None };
    if !pin(Some(0)) {
        p.refusal = Some(ParkRefusal::NotOnBootCpu);
        return p;
    }
    p.pin = Some(pin);
    let (stray0, deferred0) = park::stray_and_deferred();
    let r = park::stop_others(o);
    let (stray, deferred) = park::stray_and_deferred();
    p.stray_ipis = stray.wrapping_sub(stray0);
    p.guard_deferred = deferred.wrapping_sub(deferred0);
    match r {
        Ok(s) => {
            p.report = s.report();
            if lock_safe(&p.report) {
                p.stopped = Some(s);
            } else {
                drop(s);
                p.refusal = Some(ParkRefusal::ByNmi { cpus: p.report.by_nmi, first: p.report.first_nmi_cpu });
            }
        }
        Err(e) => p.refusal = Some(ParkRefusal::Stop(e)),
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clock() -> u64 {
        0
    }

    #[test]
    fn options_are_generous_under_a_hypervisor() {
        let q = stop_options(clock, true);
        let hw = stop_options(clock, false);
        assert_eq!((q.irq_wait_ns, q.nmi_wait_ns), (5_000_000_000, Some(5_000_000_000)));
        assert_eq!((hw.irq_wait_ns, hw.nmi_wait_ns), (1_000_000_000, Some(1_000_000_000)));
    }

    #[test]
    fn a_cpu_parked_by_nmi_is_not_lock_safe() {
        let ok = StopReport { others: 15, by_irq: 15, ..StopReport::default() };
        assert!(lock_safe(&ok));
        let bad = StopReport { others: 15, by_irq: 14, by_nmi: 1, first_nmi_cpu: Some(7), ..StopReport::default() };
        assert!(!lock_safe(&bad));
        let d = ParkRefusal::ByNmi { cpus: 1, first: Some(7) }.describe();
        assert!(d.contains("first: CPU 7"), "{d}");
    }

    #[test]
    fn refusals_read_as_one_line() {
        let d = ParkRefusal::Stop(StopError::NotStopped { cpus: alloc::vec![3, 9], nmi_sent: 2 }).describe();
        assert_eq!(d, "CPU(s) [3, 9] answered neither the park IPI nor the NMI (2 NMI sent)");
        assert_eq!(ParkRefusal::Stop(StopError::Busy).describe(), "Busy");
        assert!(!ParkRefusal::NotOnBootCpu.describe().contains('\n'));
    }
}
