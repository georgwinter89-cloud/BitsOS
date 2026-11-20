//! The journal commit's device work on a disk, and who waits for it.
//!
//! FS-W (generation 126): the commit block goes through alone, not behind a second flush of everything. But a
//! file's data reached the disk only through the disk's cache, so a commit's first flush wrote out every byte the
//! write-back had written since the last one, of every file; the committer holds `ext4.commit` meanwhile and every
//! other program's fsync waits for that lock (27.7 s on the Mini-PC under six writers).
//!
//! FS-W2: a step's data goes to the disk itself and the step waits for it, with no lock. A commit's flush is its
//! log blocks, what metadata was dirty, and one device flush. So another program's fsync returns while the first
//! writer's megabyte is still at a slow device: the first test, which hangs on the code before FS-W2 (there the
//! megabyte sits in the disk's cache and the second program's own commit has to write it out).

use super::disk::*;
use super::flight::{bounded, pattern, read_all, step, MIB};
use super::util::*;
use super::write::file;
use crate::dev::{is_mark, CachedDiskDev, Dev, FLUSHED};
use crate::inode::ROOT;
use block::ramdisk::RamDisk;
use block::{Dispatch, Disk, DiskKind};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::vec;

#[test]
fn another_programs_fsync_returns_while_the_first_writers_data_is_at_a_slow_device() {
    let s = Scratch::new();
    let (ld, dev, v) = disk_volume(&s, "64M", &["-b", "4096"]);
    let (a, b) = (file(&v, ROOT, "a"), file(&v, ROOT, "b"));
    v.flush().unwrap();
    let data = pattern(MIB, 2);
    let small = pattern(3 * 4096, 3);

    ld.shut.store(true, Ordering::Release);
    let _open = Open(ld.clone());
    let (w, d) = (v.clone(), data.clone());
    let first = std::thread::spawn(move || step(&w, a, 0, &d, MIB as u64));
    // The first writer's megabyte is at the device (before FS-W2: in the disk's cache, and the step is over).
    wait("the first writer's data at the device", || ld.inside.load(Ordering::Acquire) == 1 || first.is_finished());
    ld.record();
    let flushes = ld.flushes.load(Ordering::Relaxed);
    // The second program writes three pages and fsyncs: its own data, its commit, nobody else's megabyte.
    let (w, d) = (v.clone(), small.clone());
    let second = bounded("the second program's fsync", move || step(&w, b, 0, &d, d.len() as u64).and_then(|()| w.flush()));
    assert_eq!(second, Ok(()));
    assert_eq!(ld.inside.load(Ordering::Acquire), 1, "the first writer's data is not at the device any more: the test shows nothing");
    assert!(!first.is_finished());
    // What that fsync cost the device: its three pages, a few log and metadata blocks, and two device flushes
    // (the one before the commit block, which also covers its data, and the commit block's own).
    let log = ld.take_log();
    let bytes: usize = log.iter().filter(|(at, _)| !is_mark(*at)).map(|(_, d)| d.len()).sum();
    assert!(bytes < BIG, "the second program's fsync wrote {bytes} bytes");
    assert_eq!(ld.flushes.load(Ordering::Relaxed) - flushes, 2, "device flushes of one fsync");
    assert_eq!(log.last().map(|e| e.0), Some(FLUSHED));
    // Its data is on the disk already, not in the disk's cache waiting for somebody's flush.
    let blk = blocks_of(&v, b)[0] as usize;
    assert_eq!(ld.snapshot()[blk * 4096..blk * 4096 + small.len()], small[..]);

    ld.shut.store(false, Ordering::Release);
    assert_eq!(first.join().unwrap(), Ok(()));
    v.flush().unwrap();
    // The megabyte never went through the disk's cache (`read_all` below reads through it).
    assert!(dev.cached_pages() < 128, "{} pages in the disk's cache", dev.cached_pages());
    assert_eq!((read_all(&v, a), read_all(&v, b)), (data, small));
    v.sync_fs().unwrap();
    assert_clean(&s, &ld.snapshot(), "two programs, one slow data write");
}

/// The disk path: `write_through` puts its pages on the disk and leaves the other dirty pages in the cache.
#[test]
fn a_disk_writes_through_only_the_pages_asked_for() {
    const P: usize = 4096;
    let r = Arc::new(RamDisk::from_image("fswthrough", 512, vec![0u8; 64 * P], false));
    let disk = Disk::open(r.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let dev = CachedDiskDev::new(disk, 0, 64 * P as u64 / 512);
    let on_disk = |page: usize, byte: u8| r.snapshot()[page * P..(page + 1) * P].iter().all(|&b| b == byte);
    dev.write(10 * P as u64, &[0xAA; 2 * P]).unwrap();
    dev.write_through(20 * P as u64, &[0xBB; P]).unwrap();
    assert!(on_disk(20, 0xBB), "the page written through is not on the disk");
    assert!(on_disk(10, 0) && on_disk(11, 0), "a page that was only written reached the disk with it");
    let mut back = [0u8; P];
    dev.read(10 * P as u64, &mut back).unwrap();
    assert_eq!(back, [0xAA; P]);
    dev.flush().unwrap();
    assert!(on_disk(10, 0xAA) && on_disk(11, 0xAA) && on_disk(20, 0xBB));
    // Again over a page the flush left clean, and over one still dirty.
    dev.write(20 * P as u64, &[0xCC; P]).unwrap();
    dev.write_through(20 * P as u64, &[0xDD; P]).unwrap();
    dev.write_through(21 * P as u64, &[0xEE; P]).unwrap();
    assert!(on_disk(20, 0xDD) && on_disk(21, 0xEE));
    dev.flush().unwrap();
    assert!(on_disk(20, 0xDD) && on_disk(21, 0xEE) && on_disk(10, 0xAA));
}
