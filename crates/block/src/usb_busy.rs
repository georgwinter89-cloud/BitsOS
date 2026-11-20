//! GP-R3: who is still bringing USB disks up. The host controller's enumeration (xhci) and usb-msd's LUN scans
//! register a "still busy" question here, so a caller that must know "no USB disk will come" (block.usb.stick on an
//! NVMe boot) asks them instead of waiting out a fixed time; block does not depend on either crate.

use alloc::vec::Vec;
use ksync::SpinLock;

static BUSY: SpinLock<Vec<fn() -> bool>> = SpinLock::named("block.usb_busy", Vec::new());

/// Adds a "still bringing USB disks up" question (once per function; called from probe paths).
pub fn add_usb_busy(f: fn() -> bool) {
    let mut l = BUSY.lock();
    if !l.iter().any(|g| core::ptr::fn_addr_eq(*g, f)) {
        l.push(f);
    }
}

/// `None` when nobody registered (no USB host controller was probed), else whether any says busy. The questions
/// run outside the lock (they take their own).
pub(crate) fn usb_busy() -> Option<bool> {
    let l: Vec<fn() -> bool> = BUSY.lock().clone();
    if l.is_empty() { None } else { Some(l.iter().any(|f| f())) }
}

/// Linux's usb-storage waits this long after a device arrived before it scans it (`delay_use`, default 1 s:
/// drivers/usb/storage/usb.c:71, applied in usb_stor_scan_dwork's queueing at usb.c:1165-1168).
pub const SETTLE_NS: u64 = 1_000_000_000;

/// One look of a wait for USB disks. `quiet_ns`: how long neither the disk list changed nor anybody was busy;
/// `busy`: `usb_busy()`; `usb_disks`: USB disks registered now. A USB boot waits for its stick (at least one disk,
/// then 2 s of quiet, as before GP-R3); an NVMe boot needs no USB disk: done once nobody brings one up and the
/// list stood still for Linux's settle time.
pub fn done_waiting(nvme_boot: bool, quiet_ns: u64, busy: Option<bool>, usb_disks: usize) -> bool {
    if nvme_boot {
        busy != Some(true) && quiet_ns >= SETTLE_NS
    } else {
        usb_disks > 0 && quiet_ns >= 2 * SETTLE_NS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_nvme_boot_waits_only_for_the_settle_time() {
        const S: u64 = SETTLE_NS;
        // Nobody busy, quiet for the settle time: done, with or without a USB disk.
        assert!(done_waiting(true, S, Some(false), 0));
        assert!(done_waiting(true, S, Some(false), 1));
        // No USB controller at all: nothing can come.
        assert!(done_waiting(true, S, None, 0));
        // Still enumerating or scanning: go on waiting, however long it was quiet.
        assert!(!done_waiting(true, 30 * S, Some(true), 0));
        // Not yet quiet long enough.
        assert!(!done_waiting(true, S - 1, Some(false), 0));
    }

    #[test]
    fn a_usb_boot_waits_for_its_stick_as_before() {
        const S: u64 = SETTLE_NS;
        assert!(!done_waiting(false, 30 * S, Some(false), 0), "a USB boot never gives up on its stick early");
        assert!(!done_waiting(false, 2 * S - 1, Some(false), 1));
        assert!(done_waiting(false, 2 * S, Some(false), 1));
        assert!(done_waiting(false, 2 * S, Some(true), 1), "the old rule does not ask the controllers");
    }

    #[test]
    fn a_question_is_added_once() {
        fn yes() -> bool { true }
        add_usb_busy(yes);
        add_usb_busy(yes);
        assert_eq!(BUSY.lock().iter().filter(|g| core::ptr::fn_addr_eq(**g, yes as fn() -> bool)).count(), 1);
        assert_eq!(usb_busy(), Some(true));
    }
}
