//! Mounting ext4: a loop image file of another mounted file system (FS-8c: `\BITSOS2\data.img` on the boot
//! stick at `/data`) or a block disk, and the list of mounted volumes for a sync of all of them (before a
//! Reload or a power-off: the change request in the FS-8a notes).

use crate::dev::{CachedDiskDev, Dev, DiskDev, ImageDev};
use crate::vol::{Options, Volume};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use crate::errno::*;
use ksync::SpinLock;
use vfs::mount_ns::{Mount, MountNs};

static MOUNTED: SpinLock<Vec<Weak<Volume>>> = SpinLock::named("ext4.mounted", Vec::new());

/// What a mounted volume sits on: an image file (its VFS inode, kept alive by the ImageDev) or a range
/// [start, end) of a disk's blocks. Two volumes on one place would allocate the same blocks.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    Image(usize),
    Disk(usize, u64, u64),
}

impl Place {
    fn overlaps(&self, o: &Place) -> bool {
        match (*self, *o) {
            (Place::Image(a), Place::Image(b)) => a == b,
            (Place::Disk(a, s, e), Place::Disk(b, s2, e2)) => a == b && s < e2 && s2 < e,
            _ => false,
        }
    }
}

/// Places in use: None while the mount is being made, then the volume (gone when it drops).
static BUSY: SpinLock<Vec<(Place, Option<Weak<Volume>>)>> = SpinLock::named("ext4.busy", Vec::new());

/// Claims `p` for a new mount; EBUSY when a mounted (or mounting) volume uses it. Linux refuses a second
/// writable claim of a block device too (`bdev_open_by_dev` with the holder, linux-6.6/block/bdev.c); V2
/// refuses any second mount, read-only included, instead of sharing the super block.
fn claim(p: Place) -> Result<(), i64> {
    let mut b = BUSY.lock();
    b.retain(|(_, v)| v.as_ref().is_none_or(|w| w.strong_count() > 0));
    if b.iter().any(|(q, _)| q.overlaps(&p)) {
        klog::event!(Warn, "ext4", "mount.busy", place = alloc::format!("{p:?}").as_str());
        return Err(EBUSY);
    }
    b.push((p, None));
    Ok(())
}

fn settle(p: Place, v: Option<&Arc<Volume>>) {
    let mut b = BUSY.lock();
    if let Some(i) = b.iter().position(|(q, w)| *q == p && w.is_none()) {
        match v {
            Some(v) => b[i].1 = Some(Arc::downgrade(v)),
            None => {
                b.swap_remove(i);
            }
        }
    }
}

fn note(v: &Arc<Volume>) {
    let mut m = MOUNTED.lock();
    m.retain(|w| w.strong_count() > 0);
    m.push(Arc::downgrade(v));
}

/// The volumes mounted now.
pub fn mounted() -> Vec<Arc<Volume>> {
    MOUNTED.lock().iter().filter_map(Weak::upgrade).collect()
}

/// syncfs of every mounted ext4 volume (each marked clean on disk); the first error is returned.
pub fn sync_all() -> Result<(), i64> {
    let mut r = Ok(());
    for v in mounted() {
        if let Err(e) = v.sync_fs() {
            klog::event!(Warn, "ext4", "sync_all.failed", errno = e);
            r = r.and(Err(e));
        }
    }
    r
}

/// Mounts the volume on `dev` at `at`; `place` is what `dev` sits on (None: nothing shared, a test device).
pub fn mount_dev(dev: Arc<dyn Dev>, place: Option<Place>, ns: &MountNs, at: &str, opts: Options) -> Result<(Arc<Volume>, Arc<Mount>), i64> {
    if let Some(p) = place {
        claim(p)?;
    }
    let r = Volume::mount(dev, opts).and_then(|v| crate::glue::mount(&v, ns, at).map(|m| (v, m)));
    if let Some(p) = place {
        settle(p, r.as_ref().ok().map(|(v, _)| v));
    }
    let (v, m) = r?;
    note(&v);
    Ok((v, m))
}

/// Mounts the ext4 image file `image` (a path in `ns`) at `at` (a loop mount).
pub fn mount_image(ns: &MountNs, image: &str, at: &str, opts: Options) -> Result<(Arc<Volume>, Arc<Mount>), i64> {
    let p = ns.lookup_path(image.as_bytes(), true)?;
    let file = p.dentry.inode.clone();
    if !file.is_reg() {
        return Err(EINVAL);
    }
    let place = Place::Image(Arc::as_ptr(&file) as usize);
    let dev = Arc::new(ImageDev::new(file, opts.writable)?);
    klog::event!(Info, "ext4", "loop.mount", image = image, at = at, bytes = dev.size());
    mount_dev(dev, Some(place), ns, at, opts)
}

/// Mounts a whole block disk (or a partition from block `start` of `blocks` blocks) at `at`, through the disk's
/// page cache (`CachedDiskDev`).
pub fn mount_disk(disk: Arc<block::Disk>, start: u64, blocks: u64, ns: &MountNs, at: &str, opts: Options) -> Result<(Arc<Volume>, Arc<Mount>), i64> {
    let place = Place::Disk(Arc::as_ptr(&disk) as usize, start, start.saturating_add(blocks));
    mount_dev(Arc::new(CachedDiskDev::new(disk, start, blocks)), Some(place), ns, at, opts)
}

/// As `mount_disk` without a cache: every read and write is one request in order (tests of the write order).
pub fn mount_disk_direct(disk: Arc<block::Disk>, start: u64, blocks: u64, ns: &MountNs, at: &str, opts: Options) -> Result<(Arc<Volume>, Arc<Mount>), i64> {
    let place = Place::Disk(Arc::as_ptr(&disk) as usize, start, start.saturating_add(blocks));
    mount_dev(Arc::new(DiskDev::new(disk, start, blocks)), Some(place), ns, at, opts)
}
