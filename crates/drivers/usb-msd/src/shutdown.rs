//! RB-14: usb-msd's part of the storage stop before a reset (`block::shutdown`, stage `Device`). It runs after
//! every disk is closed, drained and its cache flushed (SYNCHRONIZE CACHE, Linux `sd_shutdown`); here no
//! command may start any more (not the scan's, not a raw one) and the one under way is waited for, so no
//! Bulk-Only transfer of a stick is on the bus when the host controller halts. Linux's usb-storage has no
//! shutdown hook of its own (linux-6.6/drivers/usb/storage/usb.c: suspend only); the SCSI disk above it syncs,
//! and on a restart it does not stop the unit (drivers/scsi/sd.c:3853-3859), nor does this.

use crate::disk::Storage;
use crate::usblink::UsbLink;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use block::shutdown::{LayerNote, Stage, Stop};
use ksync::Once;

struct MsdStop;

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
