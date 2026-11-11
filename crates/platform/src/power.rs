//! CP-5: what the console's `poweroff` / `reboot` commands call (IN-3 wires the command line; change request 3 in
//! plan/notes/cp-CP-5.md). Thread context: RB-15's order (`klog::shutdown`: the console mirror off the stack,
//! storage stop, park), quiesce every driver (CONTRACTS 6a), then `acpi::power`, waiting with
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

/// RB-16: how long each device's thread-context stop before the park may take (B4).
pub const STOP_BEFORE_PARK_NS: u64 = 1_000_000_000;

/// RB-16: the thread-context device stops (`devmodel::stop_all_before_park`, e.g. rtw89's chip under its own
/// lock, as Linux's `ifdown` before a reboot does) after the storage stop and before the park, with the event.
pub fn stop_before_park(what: &'static str) {
    let r = devmodel::stop_all_before_park(STOP_BEFORE_PARK_NS);
    if r.is_empty() {
        return;
    }
    let names: alloc::vec::Vec<&str> = r.iter().map(|(n, _)| n.as_str()).collect();
    let held: alloc::vec::Vec<&str> = r.iter().filter(|(_, ok)| !ok).map(|(n, _)| n.as_str()).collect();
    if held.is_empty() {
        klog::event!(Info, "power", "before_park", what = what, stopped = names.join(","));
    } else {
        klog::event!(Warn, "power", "before_park", what = what, stopped = names.join(","), not_stopped = held.join(",");
            "a device's stop before the park did not finish: its Quiesce does what it can");
    }
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
    let ended = stop_and_quiesce("poweroff", false);
    let e = power::poweroff(&wait);
    // It did not happen: the other CPUs run again (the drivers stay quiesced, the mirror stopped or, if it was
    // not live at the pause, unpaused so a later start sends normally); a later shutdown counts its steps from
    // the start again.
    drop(ended.park);
    klog::shutdown::unpause_mirror();
    klog::shutdown::reset_steps();
    // RB-16: the watchdog's normal timeout back in place of the last breadcrumb.
    klog::crumb::clear();
    e
}

/// RB-15's order (`klog::shutdown::run`): the console mirror off the network stack (its lines raw from here
/// on, the card asked until it has sent them), the storage stop (RB-14), the park, the quiesce of every
/// driver (`reboot.requested` or `off.requested` with the names), each step marked and its lines sent; the caller resets next.
fn stop_and_quiesce(what: &'static str, reboot: bool) -> klog::shutdown::Ended<hal::kexec_stub::ShutdownPark, ()> {
    klog::shutdown::run(
        what,
        &wait,
        || {
            let _ = block::shutdown::stop_all(what, block::shutdown::STOP_NS);
            stop_before_park(what);
        },
        || park_others(what),
        || {
            let devices = devmodel::quiesce_all();
            // Literal names: console's IMPORTANT list checks them in the sources.
            if reboot {
                klog::event!(Info, "power", "reboot.requested", quiesced = devices.join(","));
            } else {
                klog::event!(Info, "power", "off.requested", quiesced = devices.join(","));
            }
        },
    )
}

/// The plan for the reset after the storage stop: the full reset first when a layer asked for it (US-R).
pub fn plan_after_stop(plan: power::RebootPlan) -> power::RebootPlan {
    if !block::shutdown::cold_reset_asked() {
        return plan;
    }
    klog::event!(Warn, "power", "reboot.cold_first"; "a device was not well at the stop: the full reset comes first");
    plan.cold_first()
}

/// Resets the machine through every path of this machine's reboot plan, the triple fault last. Never returns.
pub fn reboot() -> ! {
    sync_before("reboot");
    let _ended = stop_and_quiesce("reboot", true);
    power::reboot(&plan_after_stop(power::live_reboot_plan()), &wait)
}
