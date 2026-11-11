//! RB-14: the storage stop before a machine reset or a Reload, after the write-back (`vfs::writeback::sync_all`)
//! and before the CPUs park and the drivers quiesce. Linux's restart does the same in `device_shutdown`
//! (linux-6.6/drivers/base/core.c:4724, children before parents): `sd_shutdown` syncs every disk's cache and,
//! on a restart, does not spin it down (drivers/scsi/sd.c:3838-3856); then the host controllers halt
//! (`usb_hcd_pci_shutdown` -> `xhci_shutdown`, drivers/usb/core/hcd-pci.c:359, drivers/usb/host/xhci.c:631).
//!
//! `stop_all`, newest disk first (a stacked disk, /data's image on the stick, before the stick under it):
//! 1. the disk holds new requests back (they wait for `resume_all`, or the reset ends them; their waiters keep
//!    their own deadline), what was queued before still runs;
//! 2. it waits until nothing of the disk is queued or at the device (bounded; on the deadline the queued rest
//!    fails with `Gone`, what the device holds is logged);
//! 3. its cache is flushed straight at the device when it was ever written (usb-msd: SYNCHRONIZE CACHE);
//! then the layers below in `Stage` order (usb-msd: no command runs and none can start; xHCI: no bulk transfer
//! in flight, the command ring idle). Everything is bounded by one budget (B4): a hung stick costs at most that,
//! then the reset comes anyway (the driver Quiesce halts the controller, bus master off).
//!
//! Thread context (it sleeps); never from the panic path (that one has `xhci::panic_stop_all`). `resume_all`
//! undoes it for a Reload that did not jump and for the ktests. Events: `block shutdown.disk` per disk,
//! `block shutdown.layer` per layer, `block shutdown.done` with the time it took.

use crate::disk::Disk;
use crate::error::BlockError;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use ksync::{Completion, SpinLock, WaitQueue};

/// The whole stop's budget: the stick's last flush takes milliseconds; a stick that does not answer must not
/// keep the machine from its reset much longer than the write-back's own bound did.
pub const STOP_NS: u64 = 5_000_000_000;
/// How often the stop looks whether a disk or a layer is quiet.
const POLL_NS: u64 = 1_000_000;

/// Where a layer below the disks stops: the devices before their host controllers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// A disk driver's own commands (usb-msd).
    Device,
    /// A host controller under the disk drivers (xHCI).
    Host,
}

/// What a layer's stop found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerNote {
    /// Nothing of this layer runs any more.
    pub idle: bool,
    /// For the event ("xhci0: bulk=0 interrupt=3 ...").
    pub detail: String,
}

/// A layer under the disks that must be quiet before the reset (registered once by its driver).
pub trait Stop: Send + Sync {
    fn name(&self) -> &str;
    /// Ends this layer's I/O before `deadline_ns` (`ksync::now_ns` clock); may sleep, must not wait past it.
    fn stop(&self, deadline_ns: u64) -> LayerNote;
    /// Takes I/O again.
    fn resume(&self);
}

/// How a disk's last flush went.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flushed {
    Done,
    Failed(BlockError),
    /// Never written since boot: nothing to flush (NVMe with writes off is never touched).
    NotWritten,
    /// Still at the device when the budget ran out (the helper finishes or not; the reset comes).
    TimedOut,
    /// Requests were still at the device: flushing past them would race the dispatcher.
    Busy,
}

impl Flushed {
    pub fn as_str(self) -> &'static str {
        match self {
            Flushed::Done => "done",
            Flushed::Failed(_) => "failed",
            Flushed::NotWritten => "not_written",
            Flushed::TimedOut => "timed_out",
            Flushed::Busy => "busy",
        }
    }
}

/// One disk's stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskStop {
    pub name: String,
    /// Requests queued / at the device when the disk closed.
    pub queued: usize,
    pub at_device: usize,
    /// Nothing queued or at the device afterwards.
    pub drained: bool,
    /// Queued requests failed at the deadline.
    pub failed_queued: usize,
    /// A write had completed after the last flush once the disk was drained (before its stop's flush).
    pub dirty: bool,
    pub flush: Flushed,
    /// Since the last completed write, once drained (None: never written).
    pub since_write_ms: Option<u64>,
    pub ms: u64,
}

impl DiskStop {
    /// Drained without losing a queued request, and flushed (or never written).
    pub fn quiet(&self) -> bool {
        self.drained && self.failed_queued == 0 && matches!(self.flush, Flushed::Done | Flushed::NotWritten)
    }
}

/// What `stop_all` did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub disks: Vec<DiskStop>,
    pub layers: Vec<(String, LayerNote)>,
    pub ms: u64,
    /// Every disk drained and flushed (or never written) and every layer idle.
    pub quiet: bool,
}

static LAYERS: SpinLock<Vec<(Stage, Arc<dyn Stop>)>> = SpinLock::named("block.shutdown.layers", Vec::new());
static STOPPED: AtomicBool = AtomicBool::new(false);
static COLD_RESET: AtomicBool = AtomicBool::new(false);

/// Registers a layer (drivers do it once, at their first device).
pub fn add(stage: Stage, s: Arc<dyn Stop>) {
    let mut l = LAYERS.lock();
    if l.try_reserve(1).is_err() {
        drop(l);
        klog::event!(Warn, "block", "shutdown.no_layer", layer = String::from(s.name()); "no memory: the stop before a reset will not wait for this layer");
        return;
    }
    l.push((stage, s));
}

/// The stop ran and was not undone: disks registered from now on are closed at once.
pub fn stopped() -> bool {
    STOPPED.load(Ordering::Acquire)
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// A layer found a device a warm reset may not cure (usb-msd: a stick that was not well at the stop; on 3.11. one stayed
/// wedged through the warm reset and the firmware booted Windows): the machine reset should drop the ports' power.
pub fn ask_cold_reset() {
    COLD_RESET.store(true, Ordering::Release);
}

/// Takes `ask_cold_reset` back (`resume_all` does, and a ktest that only tried a stop).
pub fn withdraw_cold_reset() {
    COLD_RESET.store(false, Ordering::Release);
}

/// Whether the reset that follows this stop should be a cold one (the reset paths read it after `stop_all`).
pub fn cold_reset_asked() -> bool {
    COLD_RESET.load(Ordering::Acquire)
}

/// Stops every disk and every layer under them (see the module text) within `budget_ns`.
pub fn stop_all(why: &str, budget_ns: u64) -> Summary {
    STOPPED.store(true, Ordering::Release);
    let layers: Vec<(Stage, Arc<dyn Stop>)> = LAYERS.lock().clone();
    stop_these(why, &crate::disks(), layers, budget_ns)
}

/// `stop_all` on given disks (registration order, the newest last) and layers (registration order).
pub(crate) fn stop_these(why: &str, disks: &[Arc<Disk>], mut layers: Vec<(Stage, Arc<dyn Stop>)>, budget_ns: u64) -> Summary {
    let t0 = now();
    let deadline = t0.saturating_add(budget_ns);
    let mut sum = Summary { quiet: true, ..Summary::default() };
    for d in disks.iter().rev() {
        let s = stop_disk(d, deadline);
        sum.quiet &= s.quiet();
        log_disk(why, &s);
        // The summary is for the caller's logs and the tests; without memory for it the stop itself still ran.
        let _ = sum.disks.try_reserve(1).map(|()| sum.disks.push(s));
    }
    // Stable: within a stage the newest registered first, as `quiesce_all` does.
    layers.reverse();
    layers.sort_by_key(|(st, _)| *st);
    for (_, l) in &layers {
        let t = now();
        let n = l.stop(deadline);
        let ms = now().saturating_sub(t) / 1_000_000;
        if n.idle {
            klog::event!(Info, "block", "shutdown.layer", why = String::from(why), layer = String::from(l.name()), idle = true, ms = ms,
                detail = n.detail.clone());
        } else {
            klog::event!(Warn, "block", "shutdown.layer", why = String::from(why), layer = String::from(l.name()), idle = false, ms = ms,
                detail = n.detail.clone(); "this layer still had I/O running when the budget ran out");
        }
        sum.quiet &= n.idle;
        let _ = sum.layers.try_reserve(1).map(|()| sum.layers.push((String::from(l.name()), n)));
    }
    sum.ms = now().saturating_sub(t0) / 1_000_000;
    if sum.quiet {
        klog::event!(Info, "block", "shutdown.done", why = String::from(why), disks = sum.disks.len(), layers = sum.layers.len(),
            quiet = true, ms = sum.ms, budget_ms = budget_ns / 1_000_000);
    } else {
        klog::event!(Warn, "block", "shutdown.done", why = String::from(why), disks = sum.disks.len(), layers = sum.layers.len(),
            quiet = false, ms = sum.ms, budget_ms = budget_ns / 1_000_000; "storage was not quiet before the reset");
    }
    sum
}

/// Undoes `stop_all`: every disk and layer takes requests again.
pub fn resume_all(why: &str) {
    let layers: Vec<(Stage, Arc<dyn Stop>)> = LAYERS.lock().clone();
    for (_, l) in layers.iter().rev() {
        l.resume();
    }
    for d in crate::disks() {
        d.reopen();
    }
    STOPPED.store(false, Ordering::Release);
    withdraw_cold_reset();
    klog::event!(Info, "block", "shutdown.resumed", why = String::from(why));
}

fn stop_disk(d: &Arc<Disk>, deadline: u64) -> DiskStop {
    let t0 = now();
    let (queued, at_device) = d.close();
    let mut drained = wait_quiet(d, deadline);
    let mut failed_queued = 0;
    if !drained {
        failed_queued = d.fail_waiting();
        drained = d.pending() == (0, 0);
    }
    // Only now: a write that was queued or at the device when the disk closed has completed during the drain
    // (the install case: the first write of the boot may be one of them), and it needs the flush.
    let (written, dirty, last_write) = d.write_state();
    let since_write_ms = written.then(|| now().saturating_sub(last_write) / 1_000_000);
    let flush = if !drained {
        Flushed::Busy
    } else if !written {
        Flushed::NotWritten
    } else {
        flush_bounded(d, deadline)
    };
    DiskStop { name: String::from(d.name()), queued, at_device, drained, failed_queued, dirty, flush, since_write_ms,
        ms: now().saturating_sub(t0) / 1_000_000 }
}

/// Nobody wakes it: a sleep of at most `POLL_NS` that looks at its condition (on the host too, where the
/// scheduler's sleep has no clock).
static POLL: WaitQueue = WaitQueue::new();

/// Waits in `POLL_NS` steps until nothing of `d` is queued or at the device, at most until `deadline`.
fn wait_quiet(d: &Disk, deadline: u64) -> bool {
    loop {
        if d.pending() == (0, 0) {
            return true;
        }
        let t = now();
        if t >= deadline {
            return false;
        }
        POLL.wait_until_timeout(|| d.pending() == (0, 0), POLL_NS.min(deadline - t));
    }
}

/// The flush in a helper thread, waited for until `deadline` (B4: SYNCHRONIZE CACHE on a stick that stopped
/// answering waits for usb-msd's own command timeout). Without a thread it runs here.
fn flush_bounded(d: &Arc<Disk>, deadline: u64) -> Flushed {
    let slot = Arc::new((SpinLock::named("block.shutdown.flush", None::<Result<(), BlockError>>), Completion::new()));
    let (s, disk) = (slot.clone(), d.clone());
    let name = alloc::format!("blk-stop:{}", d.name());
    let started = sched::spawn(&name, sched::Class::Normal, move || {
        let r = disk.shutdown_flush();
        *s.0.lock() = Some(r);
        s.1.complete();
    });
    if started.is_err() {
        return match d.shutdown_flush() {
            Ok(()) => Flushed::Done,
            Err(e) => Flushed::Failed(e),
        };
    }
    if !slot.1.wait_timeout(deadline.saturating_sub(now())) {
        return Flushed::TimedOut;
    }
    match slot.0.lock().take() {
        Some(Ok(())) => Flushed::Done,
        Some(Err(e)) => Flushed::Failed(e),
        None => Flushed::TimedOut,
    }
}

fn log_disk(why: &str, s: &DiskStop) {
    let quiet = s.quiet();
    let error = match s.flush {
        Flushed::Failed(e) => e.as_str(),
        _ => "",
    };
    let since = s.since_write_ms.map_or(-1, |v| v as i64);
    if quiet {
        klog::event!(Info, "block", "shutdown.disk", why = String::from(why), disk = s.name.clone(), queued = s.queued,
            at_device = s.at_device, drained = s.drained, dirty = s.dirty, flush = s.flush.as_str(), since_write_ms = since, ms = s.ms);
    } else {
        klog::event!(Warn, "block", "shutdown.disk", why = String::from(why), disk = s.name.clone(), queued = s.queued,
            at_device = s.at_device, drained = s.drained, failed_queued = s.failed_queued, dirty = s.dirty, flush = s.flush.as_str(),
            error = error, since_write_ms = since, ms = s.ms; "this disk was not quiet before the reset");
    }
}
