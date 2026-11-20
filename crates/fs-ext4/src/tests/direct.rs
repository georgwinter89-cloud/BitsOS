//! FS-W2, ext4's device on a disk (`CachedDiskDev`): file data goes to the disk beside the disk's cache, and the
//! cache stays right about those blocks. Each stale-page case here fails when `PageCache::write_direct` only runs
//! its write (checked, see plan/spec/storage.md §2.4): an old dirty page (a block that was metadata) written over
//! the new data by the next flush, an old clean page read instead of it, a write-back of the block at the device
//! landing after it, a read during the device write leaving old bytes behind.

use super::disk::*;
use crate::dev::{is_mark, CachedDiskDev, Dev, FLUSHED};
use block::{Dispatch, Disk, DiskKind, PageSeg};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::vec;
use std::vec::Vec;

const P: usize = 4096;
const PAGES: usize = 256;

fn at(page: usize) -> u64 {
    (page * P) as u64
}

fn on_disk(ld: &LogDisk, page: usize) -> Vec<u8> {
    ld.snapshot()[page * P..(page + 1) * P].to_vec()
}

fn through_cache(dev: &CachedDiskDev, page: usize) -> Vec<u8> {
    let mut b = vec![0u8; P];
    dev.read(at(page), &mut b).unwrap();
    b
}

/// The writes of a log (marks left out) as (page, first byte).
fn writes(log: &crate::dev::WriteLog) -> Vec<(usize, u8)> {
    log.iter().filter(|(a, _)| !is_mark(*a)).map(|(a, d)| (*a as usize / P, d[0])).collect()
}

#[test]
fn data_reaches_the_disk_at_once_and_every_flush_flushes_the_device_once() {
    let (ld, dev) = stack(vec![0u8; PAGES * P]);
    ld.record();
    dev.write(at(10), &[0xAA; 2 * P]).unwrap();
    dev.write_data(at(20), &[0xBB; P]).unwrap();
    assert_eq!(on_disk(&ld, 20), [0xBB; P], "the data is not on the disk when the call returns");
    assert_eq!(on_disk(&ld, 10), [0; P], "a page only written to the cache reached the disk with it");
    assert_eq!(ld.flushes.load(Ordering::Relaxed), 0, "a data write flushed the device");
    assert_eq!(dev.cached_pages(), 2, "the data went into the disk's cache");
    assert_eq!(through_cache(&dev, 20), [0xBB; P]);
    dev.flush().unwrap();
    assert_eq!((on_disk(&ld, 10), on_disk(&ld, 11)), (vec![0xAA; P], vec![0xAA; P]));
    assert_eq!(ld.flushes.load(Ordering::Relaxed), 1, "one device flush per flush");
    // Nothing dirty in the cache, data only beside it: the flush must still reach the device, behind the data.
    dev.write_data(at(21), &[0xCC; P]).unwrap();
    dev.flush().unwrap();
    assert_eq!(ld.flushes.load(Ordering::Relaxed), 2, "a flush with no dirty page did not flush the device: data written beside the cache would never be durable");
    let log = ld.take_log();
    assert_eq!(log.last().map(|e| e.0), Some(FLUSHED));
    assert_eq!(writes(&log).last(), Some(&(21, 0xCC)));
    // The commit block's way: its page and one device flush, also over a page the last flush left clean.
    dev.write_through(at(30), &[0xDD; P]).unwrap();
    dev.write_through(at(30), &[0xEE; P]).unwrap();
    assert_eq!(on_disk(&ld, 30), [0xEE; P]);
    assert_eq!(ld.flushes.load(Ordering::Relaxed), 4);
}

/// A block that was metadata: its home write sits dirty in the disk's cache; the block is freed and becomes file
/// data. The old page goes to the disk before the data, never after, and the next flush has nothing for the block.
#[test]
fn a_dirty_page_of_a_block_that_was_metadata_does_not_land_on_the_file_data() {
    let (ld, dev) = stack(vec![0u8; PAGES * P]);
    ld.record();
    dev.write(at(40), &[0x11; P]).unwrap();
    dev.write(at(50), &[0x22; P]).unwrap();
    dev.write_data(at(40), &[0xAA; P]).unwrap();
    assert_eq!(writes(&ld.take_log()), [(40, 0x11), (40, 0xAA)], "the old page first, then the data; nothing else written for it");
    ld.record();
    dev.flush().unwrap();
    assert_eq!(writes(&ld.take_log()), [(50, 0x22)], "the flush wrote the block again");
    assert_eq!(on_disk(&ld, 40), [0xAA; P], "the old metadata page is on the disk over the file's data");
    assert_eq!(through_cache(&dev, 40), [0xAA; P]);
}

/// The same block seen through a read before (a directory block read at a lookup; file data read through the
/// disk's cache): the clean page is not what a later read gets.
#[test]
fn a_clean_page_of_the_block_is_not_read_instead_of_the_file_data() {
    let (ld, dev) = stack((0..PAGES * P).map(|i| (i / P) as u8).collect());
    assert_eq!(through_cache(&dev, 60), [60; P]);
    dev.write_data(at(60), &[0xAA; 2 * P]).unwrap();
    let mut b = [0u8; P];
    assert_eq!(dev.read_cached(at(60), &mut b), Ok(false), "the old page is still cached");
    assert_eq!((through_cache(&dev, 60), through_cache(&dev, 61), through_cache(&dev, 62)), (vec![0xAA; P], vec![0xAA; P], vec![62; P]));
    assert_eq!(on_disk(&ld, 60), [0xAA; P]);
}

/// The reverse: a block that held file data (written beside the cache) is freed and becomes metadata, written
/// through the cache. With 4 KiB blocks the metadata write replaces the whole page, so nothing of the data's time
/// can be left in it; a later direct write of the block (data again) meets the dirty metadata page as above.
#[test]
fn a_block_that_was_file_data_takes_metadata_through_the_cache_and_data_again() {
    let (ld, dev) = stack(vec![0u8; PAGES * P]);
    dev.write_data(at(70), &[0xAA; P]).unwrap();
    assert_eq!(through_cache(&dev, 70), [0xAA; P]);
    dev.write(at(70), &[0x11; P]).unwrap();
    assert_eq!(through_cache(&dev, 70), [0x11; P]);
    assert_eq!(on_disk(&ld, 70), [0xAA; P]);
    dev.flush().unwrap();
    assert_eq!(on_disk(&ld, 70), [0x11; P]);
    dev.write(at(70), &[0x12; P]).unwrap();
    dev.write_data(at(70), &[0xBB; P]).unwrap();
    dev.flush().unwrap();
    assert_eq!((on_disk(&ld, 70), through_cache(&dev, 70)), (vec![0xBB; P], vec![0xBB; P]));
}

/// A flush has the block's old page at the device (the page is clean meanwhile) when the data write begins: the
/// data write waits for it, so the old bytes land first. Two things hold that order on this stack, the cache's
/// write-back lock (`write_direct`) and the block layer, which lets no request overtake an older one it overlaps
/// (`block::queue`); so this test also passes with `write_direct` taken out, like the next one. It shows the
/// stack's order. That `write_direct` alone holds it is `vfs::fsw2_tests`'
/// `a_write_back_at_the_device_lands_before_the_direct_write`, whose backend orders nothing.
#[test]
fn a_flush_of_the_old_page_at_the_device_lands_before_the_data() {
    let (ld, dev) = stack(vec![0u8; PAGES * P]);
    let _open = Open(ld.clone());
    ld.record();
    dev.write(at(80), &[0x11; P]).unwrap();
    ld.hold_next(at(80), at(81));
    let d = dev.clone();
    let flush = std::thread::spawn(move || d.flush());
    wait("the flush's write at the device", || ld.held.load(Ordering::Acquire) == 1);
    let d = dev.clone();
    let data = std::thread::spawn(move || d.write_data(at(80), &[0xAA; P]));
    std::thread::sleep(Duration::from_millis(100));
    let early = data.is_finished();
    ld.hold_shut.store(false, Ordering::Release);
    assert_eq!(flush.join().unwrap(), Ok(()));
    assert_eq!(data.join().unwrap(), Ok(()));
    assert!(!early, "the data write did not wait for the write-back of its block at the device");
    assert_eq!(writes(&ld.take_log()), [(80, 0x11), (80, 0xAA)]);
    assert_eq!(on_disk(&ld, 80), [0xAA; P], "the old page landed on the data");
}

/// A read of the blocks through the disk's cache that begins while their data is at the device: the block layer
/// lets no request overtake an older one it overlaps when either writes (`block::queue`), so the read waits and
/// gets the data. What is left for the cache to guard is a read that took old bytes from the device *before* the
/// data write and inserts them after it; the device cannot be held in that state from outside (the write would
/// wait for the read), `vfs::fsw2_tests` drives it with a backend of its own
/// (`a_fill_that_read_old_bytes_inserts_nothing_after_the_direct_write`).
#[test]
fn a_read_that_begins_during_the_device_write_gets_the_data() {
    let (ld, dev) = stack((0..PAGES * P).map(|i| (i / P) as u8).collect());
    let _open = Open(ld.clone());
    ld.shut.store(true, Ordering::Release);
    let d = dev.clone();
    let data = std::thread::spawn(move || d.write_data(at(100), &vec![0xAA; BIG]));
    ld.wait_inside(1);
    let d = dev.clone();
    let read = std::thread::spawn(move || through_cache(&d, 101));
    std::thread::sleep(Duration::from_millis(50));
    ld.shut.store(false, Ordering::Release);
    assert_eq!(data.join().unwrap(), Ok(()));
    assert_eq!(read.join().unwrap(), [0xAA; P]);
    assert_eq!(through_cache(&dev, 101), [0xAA; P]);
    assert_eq!(on_disk(&ld, 100 + BIG / P - 1), [0xAA; P]);
}

/// The file's pages as the request's memory, and the fallback for a partition that begins inside a page of the
/// disk's cache (its data keeps going through the cache; `takes_pages` says so and the write-back copies).
#[test]
fn pages_go_to_the_disk_themselves_and_through_the_cache_when_the_partition_is_not_page_aligned() {
    let file = vfs::pagecache::PageCache::tmpfs(Arc::new(vfs::pool::Pool::new("fsw2-direct")));
    let bytes: Vec<u8> = (0..3 * P).map(|i| (i % 251) as u8).collect();
    assert_eq!(file.write_at(0, &bytes), Ok(bytes.len()));
    let segs = || (0..3).map(|i| PageSeg { page: file.cached(i).unwrap(), off: 0, len: P }).collect::<Vec<_>>();

    let (ld, dev) = stack(vec![0u8; PAGES * P]);
    assert!(dev.takes_pages());
    dev.write_pages(at(120), segs()).unwrap();
    assert_eq!(ld.snapshot()[120 * P..123 * P], bytes[..]);
    assert_eq!(dev.cached_pages(), 0);

    let ld = LogDisk::new(vec![0u8; PAGES * P]);
    let disk = Disk::open(ld.clone(), DiskKind::Ram, Dispatch::Inline).unwrap();
    let part = CachedDiskDev::new(disk, 3, (PAGES * P / 512 - 3) as u64);
    assert!(!part.takes_pages());
    part.write_pages(at(120), segs()).unwrap();
    part.write_data(at(130), &bytes).unwrap();
    assert_eq!(ld.snapshot()[120 * P + 1536..123 * P + 1536], [0u8; 3 * P][..], "through the cache: not on the disk before a flush");
    let mut back = vec![0u8; 3 * P];
    part.read(at(120), &mut back).unwrap();
    assert_eq!(back, bytes);
    part.flush().unwrap();
    assert_eq!(ld.snapshot()[120 * P + 1536..123 * P + 1536], bytes[..]);
    assert_eq!(ld.snapshot()[130 * P + 1536..133 * P + 1536], bytes[..]);
}
