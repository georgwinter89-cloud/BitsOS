//! RB-14: usb-msd's part of the storage stop before a reset (`block::shutdown`, stage `Device`). It runs after
//! every disk is closed, drained and its cache flushed (SYNCHRONIZE CACHE, Linux `sd_shutdown`); here no
//! command may start any more (not the scan's, not a raw one) and the one under way is waited for, so no
//! Bulk-Only transfer of a stick is on the bus when the host controller halts. Linux's usb-storage has no
//! shutdown hook of its own (linux-6.6/drivers/usb/storage/usb.c: suspend only); the SCSI disk above it syncs,
//! and on a restart it does not stop the unit (drivers/scsi/sd.c:3853-3859), nor does this.
//!
//! US-R: a device that is not well at this point (a command never ended, a recovery under way or given up) gets a
//! clean stop on top: its port's power off and on where the port can do that, and a warm reset, so that the firmware
//! finds it fresh from a reset. 3.11.: a stick wedged by a stall stayed wedged through the machine's warm reset and
//! the firmware booted Windows. A healthy stick is left as it is: that is the path every reboot so far has proved.

use crate::disk::Storage;
use crate::usblink::UsbLink;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::shutdown::{LayerNote, Stage, Stop};
use ksync::Once;
use usb_core::ResetKind;

struct MsdStop;

/// What the clean stop of a sick device may take at most, whatever the stop's budget left: a wedged stick has spent
/// that budget on its flush by now, and this is the reset that decides whether the firmware finds it.
const CLEAN_STOP_NS: u64 = 3_000_000_000;

klog::counter!(CLEAN_STOPS, "usb.msd.clean_stops");

/// A sick device's clean stop. A stick can stay wedged through any reset that leaves its power on, and a port's power
/// switch may do nothing on this board: a machine reset that drops the ports' power is what surely helps
/// (`block::shutdown::ask_cold_reset`, which the reset paths read).
fn clean_stop(s: &Arc<Storage<UsbLink>>) {
    block::shutdown::ask_cold_reset();
    CLEAN_STOPS.inc();
    let Some(dev) = crate::device_of(s) else { return };
    let r = dev.reset_device(ResetKind::ForFirmware, CLEAN_STOP_NS);
    klog::event!(Warn, "usb", "msd.clean_stop", device = s.path.clone(), ok = r.is_ok(), error = r.err().map_or("", |e| e.as_str());
        "this device was not well; its port was reset for the firmware");
}

fn now() -> u64 {
    ksync::now_ns().unwrap_or(0)
}

/// Stops each device; the note names the ones still busy at the deadline.
pub(crate) fn stop_storages(list: &[Arc<Storage<UsbLink>>], deadline_ns: u64) -> LayerNote {
    let mut busy: Vec<String> = Vec::new();
    for s in list {
        let t = now();
        let idle = s.stop_io(deadline_ns, &now);
        klog::event!(Info, "usb", "msd.stopped", device = s.path.clone(), idle = idle, waited_ms = now().saturating_sub(t) / 1_000_000);
        if !idle {
            busy.push(s.path.clone());
        }
        if !idle || s.unhealthy() {
            clean_stop(s);
        }
    }
    let detail = if busy.is_empty() { format!("devices={}", list.len()) } else { format!("devices={} busy={}", list.len(), busy.join(",")) };
    LayerNote { idle: busy.is_empty(), detail }
}

impl Stop for MsdStop {
    fn name(&self) -> &str {
        "usb-msd"
    }
    fn stop(&self, deadline_ns: u64) -> LayerNote {
        stop_storages(&crate::storages(), deadline_ns)
    }
    fn resume(&self) {
        for s in crate::storages() {
            s.resume_io();
        }
    }
}

static REGISTERED: Once<()> = Once::new();

/// At the first device: the hook goes into the stop list once; a device attached after the stop starts stopped.
pub(crate) fn attach(s: &Storage<UsbLink>) {
    REGISTERED.get_or_init(|| block::shutdown::add(Stage::Device, Arc::new(MsdStop)));
    if block::shutdown::stopped() {
        let _ = s.stop_io(0, &now);
    }
}
