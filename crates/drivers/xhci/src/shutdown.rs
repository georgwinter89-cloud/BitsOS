//! RB-14: the xHCI part of the storage stop before a reset (`block::shutdown`, stage `Host`, after usb-msd
//! started no more commands and waited for its last one): the controller's transfers in flight per kind, and a
//! bounded wait until no bulk transfer is live and the command ring is idle. It does not halt the controller:
//! that stays the lock-free `Quiesce` (RS off, HCH, bus master off; Linux `xhci_shutdown` halts too,
//! linux-6.6/drivers/usb/host/xhci.c:631-662), so the reset path keeps one order and the HID pipes run until
//! then. A zombie TD (an endpoint that did not stop for a cancel, `bulk.rs`) may still be run by the controller:
//! it is counted and named, the halt in `Quiesce` ends it.

use crate::hc::{Controller, Env};
use crate::queue::TdState;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::shutdown::{LayerNote, Stage, Stop};
use core::sync::atomic::Ordering;
use usb_core::EpKind;

/// Never longer than this, whatever the budget left: what usb-msd waited for is over, only a stray transfer
/// could still run, and the halt follows anyway.
pub const IDLE_WAIT_NS: u64 = 500_000_000;
const POLL_NS: u64 = 1_000_000;

/// The transfers of one controller, counted under its locks (thread context).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Activity {
    /// Live or cancelling TDs on bulk endpoints (storage).
    pub bulk: usize,
    pub interrupt: usize,
    pub isoch: usize,
    /// TDs done for their owner but still on a ring whose endpoint did not stop.
    pub zombies: usize,
    /// A control transfer under way on some slot.
    pub control: usize,
    /// A command is on the command ring.
    pub command: bool,
}

impl Activity {
    /// Nothing that moves storage data: no bulk TD, no zombie, no control transfer, no command.
    pub fn storage_idle(&self) -> bool {
        self.bulk == 0 && self.zombies == 0 && self.control == 0 && !self.command
    }
    pub fn describe(&self) -> String {
        format!("bulk={} interrupt={} isoch={} zombies={} control={} command={}", self.bulk, self.interrupt, self.isoch,
            self.zombies, self.control, self.command)
    }
}

impl<E: Env> Controller<E> {
    pub fn activity(&self) -> Activity {
        let mut a = Activity { command: self.cmd_busy.load(Ordering::Acquire), ..Activity::default() };
        let slots: Vec<_> = self.slots.lock().iter().flatten().cloned().collect();
        for s in slots {
            let x = s.xfer.lock();
            if x.ctl.is_some() {
                a.control += 1;
            }
            for r in x.eps.iter().flatten() {
                let kind = r.ep.map(|e| e.kind());
                for td in &r.tds {
                    match (td.state, kind) {
                        (TdState::Zombie, _) => a.zombies += 1,
                        (_, Some(EpKind::Bulk)) => a.bulk += 1,
                        (_, Some(EpKind::Interrupt)) => a.interrupt += 1,
                        (_, Some(EpKind::Isoch)) => a.isoch += 1,
                        _ => {}
                    }
                }
            }
        }
        a
    }

    /// Waits (sleeping, at most until `deadline_ns` and `IDLE_WAIT_NS`) until `storage_idle`; returns the last look.
    pub fn wait_storage_idle(&self, deadline_ns: u64, now: &dyn Fn() -> u64, sleep: &dyn Fn(u64)) -> (Activity, u64) {
        let t0 = now();
        let end = deadline_ns.min(t0.saturating_add(IDLE_WAIT_NS));
        loop {
            let a = self.activity();
            if a.storage_idle() || now() >= end {
                return (a, now().saturating_sub(t0));
            }
            sleep(POLL_NS);
        }
    }
}

struct HostStop;

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

impl Stop for HostStop {
    fn name(&self) -> &str {
        "xhci"
    }
    fn stop(&self, deadline_ns: u64) -> LayerNote {
        let mut parts = Vec::new();
        let mut idle = true;
        for c in crate::controllers() {
            let (a, waited) = c.wait_storage_idle(deadline_ns, &now, &|ns| sched::sleep_ns(ns));
            klog::event!(Info, "usb", "xhci.storage_idle", ctrl = c.name.as_str(), idle = a.storage_idle(), state = c.state().as_str(),
                bulk = a.bulk, interrupt = a.interrupt, isoch = a.isoch, zombies = a.zombies, control = a.control, command = a.command,
                waited_ms = waited / 1_000_000);
            idle &= a.storage_idle();
            parts.push(format!("{}: {}", c.name, a.describe()));
        }
        LayerNote { idle, detail: parts.join("; ") }
    }
    fn resume(&self) {}
}

/// Once, at the first controller.
pub(crate) fn register_once() {
    static ONCE: ksync::Once<()> = ksync::Once::new();
    ONCE.get_or_init(|| block::shutdown::add(Stage::Host, Arc::new(HostStop)));
}
