//! NVMe, read side (NV-1; V2-PLAN §8a.8, R8a). Owner: NV.
//!
//! - `regs`, `cmd`, `queue`, `prp`, `ident`: registers, the command builder (no media-writing opcode exists),
//!   the ring logic, PRP lists and the Identify/SMART layouts, as plain logic (host-tested);
//! - `ctrl::Controller<H>`: bring-up, admin and I/O queue (polled first, then the MSI-X vector), Read, Flush,
//!   Get Log Page, recovery after a timeout, Quiesce; `mmio_hw::MmioHw` is the kernel's `Hw`, `fake` (tests)
//!   a register-level fake controller;
//! - `ns::NsDevice`: a namespace as `C: BlockDevice` (read-only), registered with FS-6's block layer as
//!   `DiskKind::Nvme`; its partitions (`gpt`) as `DiskKind::Stacked` disks on top, so every write passes the
//!   whole disk's refusal hook (no `nvme_write`, Windows/EFI partitions);
//! - `tool`: the `nvme` MCP tool (read-only); `ktests`: `nv.*`.
//!
//! Probe maps BAR0 and starts a thread per controller (`nvme-init:<name>`, class Normal) that brings it up and
//! registers its disks: the block layer reads each partition table through the new queue, which waits.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod cmd;
pub mod ctrl;
#[cfg(test)]
mod fake;
/// Partitions as block devices (NV's file in FS's crate).
pub use block::gpt;
pub mod hw;
pub mod ident;
mod ktests;
mod mmio_hw;
pub mod ns;
pub mod prp;
pub mod queue;
pub mod regs;
#[cfg(test)]
mod tests;
mod tool;

pub use ctrl::{CmdError, Config, Controller, Info, Namespace, State};
pub use mmio_hw::MmioHw;

use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use devmodel::Quiesce;
use devmodel::block::BlockDevice;
use ksync::SpinLock;

pub type Nvme = Controller<MmioHw>;

/// NVM Express by class (01h/08h/02h), as Linux matches PCI_CLASS_STORAGE_EXPRESS (drivers/nvme/host/pci.c).
static MATCHES: [pci::Match; 1] = [pci::Match::Class { class: 0x01_08_02, mask: 0xFF_FF_FF }];

pci::pci_driver!(pci::DriverEntry { name: "nvme", matches: &MATCHES, opt_in_key: None, probe });

struct Entry {
    ctrl: Arc<Nvme>,
    /// Block disks registered for it (each namespace before its partitions).
    disks: Vec<String>,
}

static DEVICES: SpinLock<Vec<Entry>> = SpinLock::named("nvme.devices", Vec::new());
static NEXT: AtomicUsize = AtomicUsize::new(0);
/// Controllers whose bring-up thread has not finished yet.
static PENDING: AtomicUsize = AtomicUsize::new(0);

/// Every controller probed, in probe order.
pub fn controllers() -> Vec<Arc<Nvme>> {
    DEVICES.lock().iter().map(|e| e.ctrl.clone()).collect()
}

/// The block disks a controller registered (namespaces and partitions).
pub fn disks_of(name: &str) -> Vec<String> {
    DEVICES.lock().iter().find(|e| e.ctrl.name == name).map(|e| e.disks.clone()).unwrap_or_default()
}

/// Waits until every bring-up thread finished (ktests, the MCP tool), at most `timeout_ns`. False on time-out.
pub fn settle(timeout_ns: u64) -> bool {
    let end = time::now_ns().saturating_add(timeout_ns);
    while PENDING.load(Ordering::Acquire) != 0 {
        if time::now_ns() >= end {
            return false;
        }
        sched::sleep_ns(5_000_000);
    }
    true
}

/// The test images' bytes (host/tests/nv makes them the same way, the fake too): byte at offset `off` of an
/// image made with `seed` = ((off >> 9) * 31 + (off & 511) * 7 + seed) mod 256.
pub fn test_pattern(seed: u8, off: u64) -> u8 {
    ((off >> 9).wrapping_mul(31).wrapping_add((off & 511) * 7).wrapping_add(seed as u64)) as u8
}

/// CONFIG `nvme_irq=off` keeps every controller polled (a way back if MSI-X misbehaves on a machine).
fn config() -> Config {
    let mut c = Config::default();
    if pci::registry::config_value("nvme_irq").is_some_and(|v| v.trim() == "off") {
        c.irqs = false;
    }
    c
}

struct Q(Arc<Nvme>);
impl Quiesce for Q {
    fn name(&self) -> &str {
        &self.0.name
    }
    fn quiesce(&self) {
        self.0.quiesce()
    }
}

fn probe(f: &pci::PciFunction) -> Result<(), String> {
    let function = format!("{}", f.addr);
    // MSI and bus master off before anything else: a controller left running by the firmware or the previous
    // kernel must DMA nowhere until its queues are ours (PC-2 `disable_device`).
    let _ = f.disable_device();
    klog::event!(Info, "nvme", "probe", function = function.as_str(), vendor = klog::Hex(f.vendor as u64), device = klog::Hex(f.device as u64));
    let hw = MmioHw::map(f).map_err(|e| {
        klog::event!(Error, "nvme", "probe_failed", function = function.as_str(), error = e.as_str());
        e
    })?;
    let name = format!("nvme{}", NEXT.fetch_add(1, Ordering::AcqRel));
    let ctrl = Arc::new(Controller::new(name.clone(), hw, config()));
    devmodel::register_quiesce(Arc::new(Q(ctrl.clone())));
    DEVICES.lock().push(Entry { ctrl: ctrl.clone(), disks: Vec::new() });
    let weak = Arc::downgrade(&ctrl);
    f.set_remove(move |_| {
        if let Some(c) = weak.upgrade() {
            remove(&c);
        }
    });
    PENDING.fetch_add(1, Ordering::AcqRel);
    let c = ctrl.clone();
    let job = move || {
        let up = c.start().is_ok();
        if up {
            register_disks(&c);
        }
        PENDING.fetch_sub(1, Ordering::AcqRel);
        if up && pci::registry::opted_in("nvme_busy") {
            busy(&c);
        }
    };
    if let Err(e) = sched::spawn(&format!("nvme-init:{name}"), sched::Class::Normal, job) {
        klog::event!(Error, "nvme", "init_thread_failed", ctrl = name.as_str(), error = format!("{e:?}"); "controller stays down");
        PENDING.fetch_sub(1, Ordering::AcqRel);
    }
    Ok(())
}

/// CONFIG `nvme_busy=on`, for the Reload scenario only (off by default; when off it costs one CONFIG look per
/// controller start): the init thread then reads the first usable namespace without pause until the controller
/// is gone, so every Reload quiesces the controller with a read in flight.
fn busy(c: &Arc<Nvme>) {
    let info = c.info();
    let Some(ns) = info.namespaces.iter().find(|n| n.unusable.is_none()) else { return };
    let bs = ns.block_size() as usize;
    let n = (info.max_transfer.min(128 << 10) / bs).max(1) as u64;
    if ns.id.nsze < 2 * n {
        return;
    }
    let mut buf = vec![0u8; n as usize * bs];
    klog::event!(Info, "nvme", "busy.started", ctrl = c.name.as_str(), nsid = ns.nsid, bytes = buf.len());
    let (mut lba, mut reads) = (0u64, 0u64);
    loop {
        if let Err(e) = c.read(ns.nsid, lba, &mut buf, true) {
            if e == CmdError::Gone {
                klog::event!(Info, "nvme", "busy.stopped", ctrl = c.name.as_str(), reads = reads; "controller gone");
            } else {
                klog::event!(Error, "nvme", "busy.failed", ctrl = c.name.as_str(), reads = reads, error = e.text());
            }
            return;
        }
        reads += 1;
        lba = if lba + 2 * n > ns.id.nsze { 0 } else { lba + n };
    }
}

/// Registers every usable namespace and its partitions with the block layer (may wait: table reads).
pub fn register_disks(c: &Arc<Nvme>) {
    let mut names = Vec::new();
    for ns in c.info().namespaces.iter().filter(|n| n.unusable.is_none()) {
        let Some(dev) = ns::NsDevice::new(c.clone(), ns.nsid) else { continue };
        let disk = match block::register_with(Arc::new(dev), block::DiskKind::Nvme, Vec::new()) {
            Ok(d) => d,
            Err(e) => {
                klog::event!(Warn, "nvme", "ns.register_failed", ctrl = c.name.as_str(), nsid = ns.nsid, error = e.as_str());
                continue;
            }
        };
        names.push(String::from(disk.name()));
        let lower: Arc<dyn BlockDevice> = Arc::new(block::DiskDevice(disk.clone()));
        let scan = disk.scan();
        let (parts, bad) = gpt::partitions(&lower, disk.name(), &scan.parts);
        for b in bad {
            klog::event!(Warn, "nvme", "part.skipped", disk = String::from(disk.name()), why = b);
        }
        for p in parts {
            let (num, first, blocks) = (p.number, p.first, p.blocks);
            match block::register(Arc::new(p), block::DiskKind::Stacked) {
                Ok(d) => {
                    klog::event!(Info, "nvme", "part.registered", disk = String::from(d.name()), number = num, first = first, blocks = blocks);
                    names.push(String::from(d.name()));
                }
                Err(e) => klog::event!(Warn, "nvme", "part.register_failed", disk = String::from(disk.name()), number = num, error = e.as_str()),
            }
        }
    }
    if let Some(e) = DEVICES.lock().iter_mut().find(|e| Arc::ptr_eq(&e.ctrl, c)) {
        e.disks.extend(names);
    }
}

/// Takes a controller's disks out of the block layer (partitions before their namespace) and forgets them.
pub fn unregister_disks(c: &Arc<Nvme>) {
    let names: Vec<String> = DEVICES.lock().iter_mut().find(|e| Arc::ptr_eq(&e.ctrl, c)).map(|e| core::mem::take(&mut e.disks)).unwrap_or_default();
    for n in names.iter().rev() {
        block::unregister(n);
    }
}

fn remove(c: &Arc<Nvme>) {
    klog::event!(Info, "nvme", "remove", ctrl = c.name.as_str());
    unregister_disks(c);
    c.remove();
    DEVICES.lock().retain(|e| !Arc::ptr_eq(&e.ctrl, c));
}
