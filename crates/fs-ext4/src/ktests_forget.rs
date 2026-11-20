//! ATOMIC-3: a mount whose dentry tree holds thousands of ext4 files is dropped (`Dentry::prune`): each file's drop
//! notes its inode for the volume (`Volume::forget`) under whatever lock the dropper holds. Before, the list grew
//! there from nothing to 16 KiB, a heap block the kernel could not reclaim for (the Mini-PC's "memory allocation
//! of 8192 bytes failed"); now each inode brings its own place from `Volume::hold`.
//!
//! - `fs.ext4.forget_many`: 3000 files made on a fresh volume, its mount dropped; `mm.heap.atomic_large` (the
//!   detector) does not grow while the tree goes, and the next call on the volume lets go of every inode. The
//!   counter is the whole kernel's: a round that saw another thread's request is run again (three rounds; the
//!   old list grew in every one).

use crate::mkfs::{self, MkfsOptions};
use crate::{mount, MemDev, Options};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use ktest::{kassert, ktest, TestResult};

const FILES: usize = 3000;
const ROUNDS: usize = 3;

fn e<T: core::fmt::Debug>(x: T) -> String {
    format!("{x:?}")
}

fn atomic_large() -> u64 {
    klog::metrics::snapshot().find(|(n, _)| *n == "mm.heap.atomic_large").map_or(0, |(_, v)| v)
}

/// One round: the counter's growth while the tree went.
fn round(r: usize) -> Result<u64, String> {
    let dev = Arc::new(MemDev::new(vec![0u8; 48 << 20]));
    let mut o = MkfsOptions::new(b"forget", [r as u8 + 3; 16], [5; 16], vfs::inode::now());
    o.zeroed = true;
    mkfs::format(&*dev, &o).map_err(e)?;
    let ns = vfs::MountNs::new_empty();
    let (v, m) = mount::mount_dev(dev, None, &ns, "/x", Options::default()).map_err(e)?;
    for i in 0..FILES {
        ns.create_file_bytes(format!("/x/f{i}").as_bytes(), 0o644, b"x").map_err(e)?;
    }
    let held = v.held_inodes();
    kassert!(held >= FILES, "round {r}: the volume holds {held} inodes for {FILES} files");
    let before = atomic_large();
    drop(m);
    drop(ns);
    let grew = atomic_large() - before;
    let left = v.held_inodes();
    kassert!(left == 0, "round {r}: {left} inodes still held after the mount went");
    Ok(grew)
}

fn forget_many() -> TestResult {
    let mut seen = vec![];
    for r in 0..ROUNDS {
        let grew = round(r)?;
        if grew == 0 {
            return Ok(());
        }
        seen.push(grew);
    }
    Err(format!("mm.heap.atomic_large grew while the tree went, in every round: {seen:?}"))
}
ktest!("fs.ext4.forget_many", Safe, forget_many);
