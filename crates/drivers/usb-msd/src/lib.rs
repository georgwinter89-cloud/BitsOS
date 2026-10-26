//! USB mass storage (owner: US; spec `plan/spec/usb.md` §5): Bulk-Only Transport and SCSI on top of US-3's bulk
//! transfer API, one `C: BlockDevice` per logical unit that is a disk.
//!
//! - `scsi`: command blocks, answers, what a sense code means;
//! - `bot`: CBW/CSW, tags, residue, phase errors, stalls, reset recovery (Linux `transport.c` rules);
//! - `link`: what the BOT layer needs from a device (`usblink::UsbLink` = the real one, chaining every stage from
//!   the previous stage's completion: V1's K-2 queued commands);
//! - `disk`: the device (`Storage`, one command at a time, a priority gate) and its units (`MsdDisk`: bring-up,
//!   sense handling, retries, medium changes, the R8 write switch, `BlockDevice`);
//! - `quirks`: transfer limits by speed and the 090C:1000 32 KiB quirk.
//!
//! The driver binds interfaces of class 08 / subclass 06 (SCSI transparent) / protocol 50h (Bulk-Only). Probe
//! runs in the controller's hotplug worker: it only opens the device and starts a `usb-msd-scan` thread (Normal)
//! that finds the LUNs (GET MAX LUN) and brings each up, so a slow stick (TEST UNIT READY up to 15 s) never
//! holds up other devices' enumeration. Disks appear in `disks()` when their scan ends and leave it at
//! disconnect; `generation()` changes with every appearance and departure (FS-6 watches it until a block
//! device registry exists, see the notes' change request).
//!
//! R8 (FS-6): every disk is registered with the block layer (`block::register`, kind Usb), whose write-refusal
//! hook decides: the boot stick only with `stick_write=on`, other USB disks only with `usb_msd_write=on` (a test
//! switch), never a disk with a Windows or EFI partition. With both switches off the units refuse writes
//! themselves too (`block::usb_writes_possible`). CONFIG `usb_msd_skip=on` keeps the driver off every device
//! (scenario `host/tests/us/40-xfer.sh` needs the sticks unbound for US-3's raw transfer tests).
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bot;
pub mod disk;
mod ktests;
mod ktests_ev;
pub mod link;
pub mod quirks;
pub mod scsi;
pub mod usblink;

#[cfg(test)]
mod sim;
#[cfg(test)]
mod tests;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use devmodel::block::BlockDevice;
use devmodel::DevError;
use ksync::SpinLock;
use usb_core::{Device, DeviceId, EpKind, Interface, UsbDriver, UsbMatch};

pub use disk::{MsdDisk, Storage};
pub use usblink::UsbLink;

/// A disk of a USB device.
pub type UsbDisk = MsdDisk<UsbLink>;

static MATCHES: [UsbMatch; 1] = [UsbMatch::Interface { class: 0x08, subclass: Some(0x06), protocol: Some(0x50) }];

pub static DRIVER: UsbDriver = UsbDriver { name: "usb-msd", matches: &MATCHES, probe, disconnect };
usb_core::usb_driver!("usb-msd", DRIVER);

struct Attached {
    device: DeviceId,
    interface: u8,
    storage: Arc<Storage<UsbLink>>,
    disks: Vec<Arc<UsbDisk>>,
}

static ATTACHED: SpinLock<Vec<Attached>> = SpinLock::named("usb.msd.attached", Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SCANNING: AtomicUsize = AtomicUsize::new(0);

klog::counter!(DEVICES, "usb.msd.devices");
klog::counter!(UNITS, "usb.msd.units");

/// Every disk of every attached device whose scan has ended.
pub fn disks() -> Vec<Arc<UsbDisk>> {
    ATTACHED.lock().iter().flat_map(|a| a.disks.iter().cloned()).collect()
}

/// The same as `C: BlockDevice`s.
pub fn block_devices() -> Vec<Arc<dyn BlockDevice>> {
    disks().into_iter().map(|d| d as Arc<dyn BlockDevice>).collect()
}

pub fn find(name: &str) -> Option<Arc<UsbDisk>> {
    disks().into_iter().find(|d| d.name() == name)
}

/// Changes whenever a disk appears or goes.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Scans (LUN bring-up) still running.
pub fn scanning() -> usize {
    SCANNING.load(Ordering::Acquire)
}

/// The attached devices (also those whose scan still runs), for tests and tools.
pub fn storages() -> Vec<Arc<Storage<UsbLink>>> {
    ATTACHED.lock().iter().map(|a| a.storage.clone()).collect()
}

fn bulk_endpoints(f: &Interface) -> Option<(u8, u8)> {
    let ep_in = f.endpoints.iter().find(|e| e.kind() == EpKind::Bulk && e.is_in())?.address;
    let ep_out = f.endpoints.iter().find(|e| e.kind() == EpKind::Bulk && !e.is_in())?.address;
    Some((ep_in, ep_out))
}

fn probe(dev: &Arc<Device>, f: &Interface) -> Result<(), String> {
    if pci::registry::opted_in("usb_msd_skip") {
        return Err("usb_msd_skip=on".into());
    }
    let (ep_in, ep_out) = bulk_endpoints(f).ok_or_else(|| String::from("no bulk IN and OUT endpoint"))?;
    let q = quirks::lookup(dev.vendor(), dev.product(), dev.desc.device);
    let max = quirks::max_bytes(dev.speed(), q);
    // FS-6: R8 is the block layer's write-refusal hook (boot stick: `stick_write`, other disks: `usb_msd_write`,
    // Windows/EFI tables never). The unit itself takes writes only when the hook could allow one at all.
    let writes = block::usb_writes_possible();
    let link = UsbLink::new(dev.clone(), f.number, ep_in, ep_out).map_err(|e| format!("no DMA memory: {}", e.as_str()))?;
    let storage = Arc::new(Storage::new(link, max, writes, q.ignore_residue).with_speed(dev.speed().as_str()));
    ATTACHED.lock().push(Attached { device: dev.id, interface: f.number, storage: storage.clone(), disks: Vec::new() });
    DEVICES.inc();
    klog::event!(Info, "usb", "msd.attached", device = storage.path.clone(), vendor = klog::Hex(dev.vendor() as u64),
        product = klog::Hex(dev.product() as u64), revision = klog::Hex(dev.desc.device as u64), speed = dev.speed().as_str(),
        ep_in = klog::Hex(ep_in as u64), ep_out = klog::Hex(ep_out as u64), max_kib = max / 1024,
        max_sectors_64 = q.max_sectors_64, writes = writes);
    if !writes {
        klog::event!(Debug, "usb", "msd.read_only", device = storage.path.clone(); "usb_msd_write is off: units read-only (R8)");
    }
    let (id, iface) = (dev.id, f.number);
    SCANNING.fetch_add(1, Ordering::AcqRel);
    let s = storage.clone();
    if let Err(e) = sched::spawn("usb-msd-scan", sched::Class::Normal, move || scan(s, id, iface)) {
        klog::event!(Warn, "usb", "msd.scan_inline", device = storage.path.clone(), error = format!("{e:?}");
            "no scan thread: scanning in the hotplug worker");
        scan(storage, id, iface);
    }
    Ok(())
}

fn scan(storage: Arc<Storage<UsbLink>>, id: DeviceId, iface: u8) {
    let r = storage.scan();
    match r {
        Ok(found) => {
            let n = found.len();
            let disks = found.clone();
            let mut kept = false;
            {
                let mut l = ATTACHED.lock();
                if let Some(a) = l.iter_mut().find(|a| a.device == id && a.interface == iface)
                    && !storage.is_gone()
                {
                    a.disks = found;
                    kept = true;
                }
            }
            if kept {
                UNITS.add(n as u64);
                GENERATION.fetch_add(1, Ordering::AcqRel);
                // FS-6: every unit goes behind the block layer's queue and write-refusal hook.
                for d in disks {
                    match block::register(d.clone(), block::DiskKind::Usb) {
                        // Unplugged meanwhile: disconnect may have looked before the disk was listed.
                        Ok(_) if storage.is_gone() => {
                            block::unregister(d.name());
                        }
                        Ok(_) => {}
                        Err(e) => klog::event!(Warn, "usb", "msd.block_register_failed", disk = String::from(d.name()), error = e.as_str()),
                    }
                }
            }
            klog::event!(Info, "usb", "msd.scanned", device = storage.path.clone(), disks = n, kept = kept);
        }
        Err(DevError::Gone) => klog::event!(Info, "usb", "msd.scan_gone", device = storage.path.clone(); "unplugged during the scan"),
        Err(e) => klog::event!(Warn, "usb", "msd.scan_failed", device = storage.path.clone(), error = format!("{e:?}")),
    }
    SCANNING.fetch_sub(1, Ordering::AcqRel);
}

fn disconnect(dev: &Arc<Device>, iface: u8) {
    let gone: Vec<Attached> = {
        let mut l = ATTACHED.lock();
        let mut out = Vec::new();
        let mut i = 0;
        while i < l.len() {
            if l[i].device == dev.id && l[i].interface == iface {
                out.push(l.remove(i));
            } else {
                i += 1;
            }
        }
        out
    };
    for a in gone {
        a.storage.set_gone();
        for d in &a.disks {
            block::unregister(d.name());
            klog::event!(Info, "usb", "msd.unit_gone", disk = String::from(d.name()), device = a.storage.path.clone(), lun = d.lun());
        }
        GENERATION.fetch_add(1, Ordering::AcqRel);
        klog::event!(Info, "usb", "msd.detached", device = a.storage.path.clone(), disks = a.disks.len());
    }
}
