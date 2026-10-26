//! CP-5: what the console's `poweroff` / `reboot` commands call (IN-3 wires the command line; change request 3 in
//! plan/notes/cp-CP-5.md). Thread context: park the other CPUs (RB-quiesce), quiesce every driver (CONTRACTS 6a), then `acpi::power`, waiting with
//! the scheduler's sleep. Power-off is never an MCP tool (R8f); MC's `reboot` tool calls `acpi::power::reboot`
//! itself after its answer.

use acpi::power::{self, PowerOffError};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::WaitQueue;

/// How long the write-back before a power change may take (B4: a hung or pulled stick must not keep the
/// machine from resetting; the same deadline as MC's `reboot`).
const SYNC_DEADLINE_NS: u64 = 10_000_000_000;

/// FS-7 change request 1 (CP part): every file and file system written back before `quiesce_all` (after it,
/// no disk request can complete), in a helper thread so a dead disk costs at most `SYNC_DEADLINE_NS`.
fn sync_before(what: &'static str) {
    // RB-8: the journal writes down what is still in the ring first (at most DRAIN_NS).
    let _ = klog::journal::wait_drained(what, klog::journal::DRAIN_NS, &sched::sleep_ns);
    struct Slot {
        done: AtomicBool,
        wq: WaitQueue,
    }
    let slot = Arc::new(Slot { done: AtomicBool::new(false), wq: WaitQueue::new() });
    let s = slot.clone();
    let started = sched::spawn("power-sync", sched::Class::Normal, move || {
        let p = vfs::writeback::sync_all();
        klog::event!(Info, "power", "synced", what = what, flushed = p.flushed, failed = p.failed);
        s.done.store(true, Ordering::Release);
        s.wq.wake_all();
    });
    if started.is_err() {
        klog::event!(Warn, "power", "sync.skipped", what = what; "no thread for the write-back: going on without it");
        return;
    }
    if !slot.wq.wait_until_timeout(|| slot.done.load(Ordering::Acquire), SYNC_DEADLINE_NS) {
        klog::event!(Warn, "power", "sync.timeout", what = what, deadline_ms = SYNC_DEADLINE_NS / 1_000_000;
            "the write-back did not finish (a hung or pulled stick?): going on anyway");
    }
}

fn wait(ns: u64) {
    sched::sleep_ns(ns)
}

/// RB-quiesce: the other CPUs parked before the quiesce (Linux `native_machine_shutdown` -> `stop_other_cpus`,
/// linux-6.6/arch/x86/kernel/reboot.c:742), so no driver's Quiesce undo meets its own work running on another
/// CPU. Moves the thread to CPU 0 first. A refusal is logged and the shutdown goes on beside the running CPUs.
/// Dropping the result releases the CPUs and unpins the thread (a power-off that did not happen). Plain `event!`
/// after the park is safe: a CPU parked by the park IPI holds no spin lock (SC's park guard), and one parked only
/// by NMI is a refusal (released before the event).
pub fn park_others(what: &'static str) -> hal::kexec_stub::ShutdownPark {
    let park = hal::kexec_stub::park_for_shutdown(&hal::kexec_stub::live_stop_options(time::now_ns), sched::pin_current);
    let r = park.report();
    match park.refusal() {
        None => klog::event!(Info, "power", "cpus.parked", what = what, cpus_stopped = r.others, by_irq = r.by_irq,
            park_ipis = r.ipis, guard_deferred = park.guard_deferred(), stop_us = r.ns / 1_000),
        Some(why) => klog::event!(Warn, "power", "cpus.park_refused", what = what, error = why.describe(),
            cpus_stopped = r.others, by_nmi = r.by_nmi; "the other CPUs run on: quiescing beside them"),
    }
    park
}

/// Powers the machine off. Returns only when it did not happen (the error says why; the drivers stay quiesced,
/// "hold the power button" was logged).
pub fn poweroff() -> PowerOffError {
    sync_before("poweroff");
    let park = park_others("poweroff");
    let devices = devmodel::quiesce_all();
    klog::event!(Info, "power", "off.requested", quiesced = devices.join(","));
    let e = power::poweroff(&wait);
    // It did not happen: the other CPUs run again (the drivers stay quiesced).
    drop(park);
    e
}

/// Resets the machine through every path of this machine's reboot plan, the triple fault last. Never returns.
pub fn reboot() -> ! {
    sync_before("reboot");
    let _park = park_others("reboot");
    let devices = devmodel::quiesce_all();
    klog::event!(Info, "power", "reboot.requested", quiesced = devices.join(","));
    power::reboot(&power::live_reboot_plan(), &wait)
}
