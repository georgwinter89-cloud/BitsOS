//! Block layer (owner: FS; FS-6): every disk of the system on `C: BlockDevice` behind one request queue.
//!
//! - `register(dev, kind)` puts a device behind a `Disk` (queue, dispatcher thread, write-refusal hook), reads
//!   its partition table once and lists it; drivers call it when a disk appears (usb-msd's scan) and
//!   `unregister` when it goes. `disks()`, `find`, `generation()` for the file systems above (FS-7/8).
//! - `disk`: the queue per device with merging up to max_sectors, priorities Interactive > Normal >
//!   Background with aging (B4), completions that wake the waiters, scatter-gather into page-cache pages.
//! - `queue`: the elevator as plain logic (host-tested).
//! - `policy`: the write-refusal hook (R8a): Windows/EFI partition tables (`table`), the boot stick
//!   (CONFIG `stick_write`), other USB disks (`usb_msd_write`), NVMe (`nvme_write`), plugged-in `WriteGate`s.
//! - `bdev`: a whole disk as a page-cache file (FS-5's `Backend`).
//! - `ramdisk`: the RAM disk for tests (limits, error injection, pause, call log).
//!
//! `configure` hands in the CONFIG reader and the boot stick's identity (vfs-sys calls it from an initcall
//! with the kernel's BootInfo); until then every switch counts as off and every USB disk as the stick.
#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;
#[cfg(test)]
extern crate std;

pub mod bdev;
pub mod disk;
pub mod error;
pub mod gpt;
mod lift;
mod ktests;
mod prio_ktests;
pub mod policy;
pub mod prio;
pub mod queue;
pub mod ramdisk;
pub mod table;
pub mod testimg;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod prio_tests;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use devmodel::block::BlockDevice;
use ksync::SpinLock;

pub use disk::{Data, Disk, DiskDevice, DiskStats, Dispatch, PageSeg, Ticket};
pub use error::{BlockError, Refusal};
pub use policy::{DiskKind, StickIdentity, StickMatch, WriteCheck, WriteGate, add_global_gate, usb_writes_possible};
pub use queue::Kind;
pub use ramdisk::RamDisk;

static DISKS: SpinLock<Vec<Arc<Disk>>> = SpinLock::named("block.disks", Vec::new());
static LISTENERS: SpinLock<Vec<Arc<dyn DiskListener>>> = SpinLock::named("block.listeners", Vec::new());

/// Told when disks come and go (FS-7 mounts the boot stick when it appears). Called with no block lock held,
/// in the registering thread (usb-msd's scan thread): a listener that needs I/O spawns its own thread.
pub trait DiskListener: Send + Sync {
    fn registered(&self, d: &Arc<Disk>);
    fn unregistered(&self, d: &Arc<Disk>);
}

/// Adds a listener and tells it about every disk listed now.
pub fn add_listener(l: Arc<dyn DiskListener>) {
    LISTENERS.lock().push(l.clone());
    for d in disks() {
        l.registered(&d);
    }
}

fn listeners() -> Vec<Arc<dyn DiskListener>> {
    LISTENERS.lock().clone()
}
static GENERATION: AtomicU64 = AtomicU64::new(0);

klog::counter!(REGISTERED, "block.disks_registered");

/// Puts `dev` behind a queue and lists it. Reads the partition table (a few blocks) through the new queue
/// before it returns, so call it from a thread that may wait (usb-msd's scan thread).
pub fn register(dev: Arc<dyn BlockDevice>, kind: DiskKind) -> Result<Arc<Disk>, BlockError> {
    register_with(dev, kind, Vec::new())
}

/// As `register`, with gates for this disk only (NV: its `nvme_write` gate).
pub fn register_with(dev: Arc<dyn BlockDevice>, kind: DiskKind, gates: Vec<Arc<dyn WriteGate>>) -> Result<Arc<Disk>, BlockError> {
    let name = String::from(dev.name());
    if find(&name).is_some() {
        klog::event!(Warn, "block", "disk.duplicate", disk = name; "a disk of this name is registered: refused");
        return Err(BlockError::Invalid);
    }
    let d = Disk::open(dev, kind, Dispatch::Thread)?;
    for g in gates {
        d.add_gate(g);
    }
    let duplicate = {
        let mut l = DISKS.lock();
        if l.iter().any(|x| x.name() == d.name()) {
            true
        } else {
            l.push(d.clone());
            false
        }
    };
    if duplicate {
        d.stop();
        return Err(BlockError::Invalid);
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
    REGISTERED.inc();
    let info = d.dev_info();
    let scan = d.scan();
    let policy = d.write_policy();
    klog::event!(Info, "block", "disk.registered", disk = name, kind = kind.as_str(), blocks = info.blocks, block_size = info.block_size,
        max_blocks = info.max_blocks_per_request, read_only = info.read_only, cache = info.has_volatile_cache,
        discard = info.supports_discard, table = scan.scheme_str(), stick = disk::stick_str(d.stick()),
        writes = if policy.is_ok() { "allowed" } else { "refused" }, reason = policy.err().map_or("", |r| r.as_str()));
    for l in listeners() {
        l.registered(&d);
    }
    Ok(d)
}

/// Takes a disk out of the list and ends its queue (queued requests fail with `Gone`). False if unknown.
pub fn unregister(name: &str) -> bool {
    let gone: Vec<Arc<Disk>> = {
        let mut l = DISKS.lock();
        let mut out = Vec::new();
        let mut i = 0;
        while i < l.len() {
            if l[i].name() == name {
                out.push(l.remove(i));
            } else {
                i += 1;
            }
        }
        out
    };
    for d in &gone {
        d.stop();
        GENERATION.fetch_add(1, Ordering::AcqRel);
        let s = d.stats();
        klog::event!(Info, "block", "disk.unregistered", disk = String::from(name), requests = s.requests, errors = s.errors, refused = s.refused);
        for l in listeners() {
            l.unregistered(d);
        }
    }
    !gone.is_empty()
}

pub fn disks() -> Vec<Arc<Disk>> {
    DISKS.lock().clone()
}

pub fn find(name: &str) -> Option<Arc<Disk>> {
    DISKS.lock().iter().find(|d| d.name() == name).cloned()
}

/// Changes whenever a disk is registered or unregistered.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// The CONFIG reader (`stick_write`, `usb_msd_write`, `nvme_write`) and the boot stick's identity from
/// BootInfo. Disks registered before are matched against the stick again.
pub fn configure(config: fn(&str) -> Option<String>, stick: Option<StickIdentity>) {
    {
        let mut f = policy::FACTS.lock();
        f.config = Some(config);
        f.stick = stick;
        f.configured = true;
    }
    let sw = policy::switches();
    klog::event!(Info, "block", "configured", stick_block0 = stick.is_some_and(|s| s.block0.is_some()),
        stick_usb = stick.and_then(|s| s.usb).map_or(alloc::string::String::new(), |(v, p)| alloc::format!("{v:04x}:{p:04x}")),
        stick_id = stick.is_some_and(|s| s.id.is_some()), stick_write = sw.stick_write, usb_msd_write = sw.usb_msd_write, nvme_write = sw.nvme_write);
    for d in disks() {
        let m = d.rematch_stick();
        klog::event!(Info, "block", "disk.stick", disk = String::from(d.name()), stick = disk::stick_str(m));
    }
}

/// Whether `configure` ran.
pub fn configured() -> bool {
    policy::FACTS.lock().configured
}
