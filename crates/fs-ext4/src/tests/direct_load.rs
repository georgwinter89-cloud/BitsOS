//! FS-W2 on the disk path (`disk`): what a failing or slow disk does to a step whose data goes straight to it, a
//! freeze and an unlink with the data at the device, and six programs writing large files at once through the VFS
//! (their pages themselves are the device's requests), compared byte by byte after a replay and after a remount.

use super::disk::*;
use super::flight::{bounded, pattern, read_all, step, MIB};
use super::util::*;
use super::write::file;
use crate::errno::EIO;
use crate::inode::ROOT;
use crate::Kind;
use std::format;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use std::vec::Vec;
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

const LAYOUT: &[&str] = &["-b", "4096"];

/// A media error answers the data write itself: nothing mapped, the claim back, the volume writable, the disk's
/// cache holding nothing of the blocks; the same step goes again when the disk does.
#[test]
fn a_media_error_in_the_data_write_gives_the_claim_back_and_leaves_the_volume_writable() {
    let s = Scratch::new();
    let (ld, dev, v) = disk_volume(&s, "64M", LAYOUT);
    let f = file(&v, ROOT, "f");
    v.flush().unwrap();
    let free = v.st.lock().free_now();
    let data = pattern(MIB, 11);
    let pages = dev.cached_pages();
    ld.big.store(FAIL_IO, Ordering::Release);
    assert_eq!(step(&v, f, 0, &data, MIB as u64), Err(EIO));
    assert!(v.writable(), "a failed data write stopped the volume");
    assert_eq!(v.stat(f).unwrap().size(), 0);
    {
        let st = v.st.lock();
        assert!(st.flights.is_empty() && st.flights.claimed() == 0, "left in flight");
        assert_eq!(st.free_now(), free, "the failed step's blocks");
    }
    // A bitmap or the like may have been read for the step; none of its 256 data blocks is in the cache.
    assert!(dev.cached_pages() < pages + 8, "the failed write left {} pages in the disk's cache", dev.cached_pages() - pages);
    assert!(v.create(ROOT, b"other", 0o100644, 0, 0, Kind::Reg).is_ok());
    v.flush().unwrap();
    ld.big.store(PASS, Ordering::Release);
    assert_eq!(step(&v, f, 0, &data, MIB as u64), Ok(()));
    v.flush().unwrap();
    assert!(read_all(&v, f) == data);
    v.sync_fs().unwrap();
    let img = ld.snapshot();
    assert_clean(&s, &img, "after a media error in a data write");
    let (_d2, v2) = mount(img);
    assert!(read_all(&v2, lookup(&v2, "/f")) == data);
}

/// A data write that timed out may still be at the device (the block layer drops its answer, it cannot take the
/// command back). Its new blocks are not handed out again in this mount, so no other file's data or metadata is
/// put where the late write could land. On the disk they are free: after the next mount they are usable again.
#[test]
fn a_timed_out_data_write_keeps_its_new_blocks_out_of_use_until_the_next_mount() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "64M", LAYOUT);
    let f = file(&v, ROOT, "f");
    v.flush().unwrap();
    let free = v.st.lock().free_now();
    let data = pattern(MIB, 12);
    ld.big.store(FAIL_TIMEOUT, Ordering::Release);
    assert_eq!(step(&v, f, 0, &data, MIB as u64), Err(EIO), "a timeout answers EIO to the program");
    ld.big.store(PASS, Ordering::Release);
    assert!(v.writable(), "a timed-out data write stopped the volume");
    let kept: Vec<(u64, u64)> = {
        let st = v.st.lock();
        assert!(st.flights.is_empty(), "the step is still in flight");
        assert_eq!(st.free_now(), free - (MIB / 4096) as u64, "the timed-out step's blocks are in use again");
        assert_eq!(st.flights.claimed(), 0, "the step still holds a claim");
        st.flights.late_in(0, u64::MAX).collect()
    };
    assert_eq!(kept.iter().map(|(a, b)| b - a).sum::<u64>(), (MIB / 4096) as u64);
    // The same pages again, and another file: neither gets the blocks the late write may hit.
    assert_eq!(step(&v, f, 0, &data, MIB as u64), Ok(()));
    let g = file(&v, ROOT, "g");
    assert_eq!(step(&v, g, 0, &data, MIB as u64), Ok(()));
    for ino in [f, g] {
        for b in blocks_of(&v, ino) {
            assert!(!kept.iter().any(|&(a, e)| b >= a && b < e), "block {b} of a timed-out write was handed out again");
        }
    }
    v.sync_fs().unwrap();
    let img = ld.snapshot();
    assert_clean(&s, &img, "after a timed-out data write");
    let (_d2, v2) = mount(img);
    assert!(read_all(&v2, lookup(&v2, "/f")) == data && read_all(&v2, lookup(&v2, "/g")) == data);
    assert_eq!(v2.st.lock().free_now(), free - 2 * (MIB / 4096) as u64, "after the remount the kept blocks are free again");
}

/// Fixer round: the blocks the file already had are written by the same request, so they are kept out of use as
/// well. When the file is truncated or deleted later they are free on the disk, and still nobody gets them in this
/// mount (they would go to another file or to a directory after the next commit, and the late megabyte on top).
#[test]
fn a_timed_out_write_keeps_the_blocks_the_file_already_had_out_of_use_too() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "64M", LAYOUT);
    let f = file(&v, ROOT, "f");
    let data = pattern(MIB, 14);
    let blocks = (MIB / 4096) as u64;
    assert_eq!(step(&v, f, 0, &data, MIB as u64), Ok(()));
    v.flush().unwrap();
    let had = blocks_of(&v, f);
    assert_eq!(had.len() as u64, blocks);
    let free = v.st.lock().free_now();
    ld.big.store(FAIL_TIMEOUT, Ordering::Release);
    assert_eq!(step(&v, f, 0, &data, MIB as u64), Err(EIO));
    ld.big.store(PASS, Ordering::Release);
    assert!(v.writable());
    {
        let st = v.st.lock();
        assert_eq!(st.free_now(), free, "blocks that still belong to the file were taken off the free count");
        assert_eq!(st.flights.late_in(0, u64::MAX).map(|(a, b)| b - a).sum::<u64>(), blocks);
    }
    // The same blocks time out once more: each is kept once.
    ld.big.store(FAIL_TIMEOUT, Ordering::Release);
    assert_eq!(step(&v, f, 0, &data[..MIB / 2], MIB as u64), Err(EIO));
    ld.big.store(PASS, Ordering::Release);
    assert_eq!(v.st.lock().flights.late_in(0, u64::MAX).map(|(a, b)| b - a).sum::<u64>(), blocks);
    // The file goes. Its blocks are free on the disk and the commit is through: before the fix the next file got them.
    v.remove(ROOT, b"f", f).unwrap();
    v.flush().unwrap();
    {
        let st = v.st.lock();
        assert_eq!(st.free_blocks(), free + blocks, "the deleted file's blocks are not free on the disk");
        assert_eq!(st.free_now(), free, "blocks a late write may hit count as room");
    }
    let (g, h) = (file(&v, ROOT, "g"), file(&v, ROOT, "h"));
    for ino in [g, h] {
        assert_eq!(step(&v, ino, 0, &data, MIB as u64), Ok(()));
        v.flush().unwrap();
        for b in blocks_of(&v, ino) {
            assert!(!had.contains(&b), "block {b} of a timed-out write was handed to another file");
        }
    }
    v.sync_fs().unwrap();
    let img = ld.snapshot();
    assert_clean(&s, &img, "after a timed-out write over mapped blocks and the file's delete");
    let (_d2, v2) = mount(img);
    assert!(read_all(&v2, lookup(&v2, "/g")) == data && read_all(&v2, lookup(&v2, "/h")) == data);
    assert_eq!(v2.st.lock().free_now(), free + blocks - 2 * blocks, "after the remount the kept blocks are free again");
}

/// Fixer round (checker finding 1, decided the other way): the request that times out is not the data but the
/// write-out of an old page of the step's blocks, which must reach the disk before the data (`write_direct`; here
/// the zeroed rest of a truncated file's last block). The data write never runs. That old page's write may still
/// land, on these blocks, so they are kept out of use exactly as after a timed-out data write: given back, the
/// next file's megabyte would go there and the late page on top of it.
#[test]
fn a_timed_out_write_out_of_an_old_page_keeps_the_steps_blocks_out_of_use_as_well() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "64M", LAYOUT);
    let f = file(&v, ROOT, "f");
    let data = pattern(MIB, 15);
    assert_eq!(step(&v, f, 0, &data, MIB as u64), Ok(()));
    v.flush().unwrap();
    // Truncated inside block 10: the block's rest is zeroed through the disk's cache, a dirty page of a data block.
    v.set_size(f, 10 * 4096 + 100).unwrap();
    let edge = blocks_of(&v, f)[10];
    let big0 = ld.big_bytes.load(Ordering::Relaxed);
    ld.time_out_next(edge * 4096, (edge + 1) * 4096);
    let from = 8 * 4096;
    assert_eq!(step(&v, f, from as u64, &data[from..], MIB as u64), Err(EIO));
    assert_eq!(ld.big_bytes.load(Ordering::Relaxed), big0, "the data went to the disk although the old page before it did not");
    assert!(v.writable(), "a timed-out page write stopped the volume");
    let kept: Vec<(u64, u64)> = {
        let st = v.st.lock();
        assert!(st.flights.is_empty() && st.flights.claimed() == 0);
        st.flights.late_in(0, u64::MAX).collect()
    };
    assert!(kept.iter().any(|&(a, e)| edge >= a && edge < e), "the block of the timed-out page is not kept: {kept:?}");
    assert_eq!(kept.iter().map(|(a, b)| b - a).sum::<u64>(), ((MIB - from) / 4096) as u64, "not the whole step is kept");
    // The write-back comes again: the page first (it is still dirty), then the data; new blocks from elsewhere.
    assert_eq!(step(&v, f, from as u64, &data[from..], MIB as u64), Ok(()));
    assert!(read_all(&v, f) == data);
    let g = file(&v, ROOT, "g");
    assert_eq!(step(&v, g, 0, &data, MIB as u64), Ok(()));
    let mine = blocks_of(&v, f);
    for b in blocks_of(&v, g).into_iter().chain(mine[11..].iter().copied()) {
        assert!(!kept.iter().any(|&(a, e)| b >= a && b < e), "kept block {b} was handed out again");
    }
    v.sync_fs().unwrap();
    let img = ld.snapshot();
    assert_clean(&s, &img, "after a timed-out write-out of an old page");
    let (_d2, v2) = mount(img);
    assert!(read_all(&v2, lookup(&v2, "/f")) == data && read_all(&v2, lookup(&v2, "/g")) == data);
}

/// A snapshot's freeze while a download's data is at the disk: it waits for the step, and the frozen disk holds
/// the whole step, clean, without a replay. An unlink of a file whose data is at the disk returns at once; the
/// step's end frees the file.
#[test]
fn a_freeze_and_an_unlink_with_the_data_at_the_disk() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "64M", LAYOUT);
    let _open = Open(ld.clone());
    let (f, gone) = (file(&v, ROOT, "f"), file(&v, ROOT, "gone"));
    v.flush().unwrap();
    let free = v.st.lock().free_now();
    let data = pattern(MIB, 13);

    ld.shut.store(true, Ordering::Release);
    let (w, d) = (v.clone(), data.clone());
    let writer = std::thread::spawn(move || step(&w, f, 0, &d, MIB as u64));
    ld.wait_inside(1);
    let w = v.clone();
    let freezer = std::thread::spawn(move || w.freeze());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!freezer.is_finished(), "the freeze did not wait for the data at the disk");
    ld.shut.store(false, Ordering::Release);
    assert_eq!(writer.join().unwrap(), Ok(()));
    assert_eq!(freezer.join().unwrap(), Ok(()));
    let img = ld.snapshot();
    assert_clean(&s, &img, "the frozen disk");
    let (_d2, v2) = mount(img);
    assert!(read_all(&v2, lookup(&v2, "/f")) == data, "the step is not whole in the frozen image");
    v.thaw();

    ld.shut.store(true, Ordering::Release);
    let (w, d) = (v.clone(), data.clone());
    let writer = std::thread::spawn(move || step(&w, gone, 0, &d, MIB as u64));
    ld.wait_inside(1);
    let w = v.clone();
    assert_eq!(bounded("an unlink", move || w.remove(ROOT, b"gone", gone)), Ok(()));
    ld.shut.store(false, Ordering::Release);
    let _ = writer.join().unwrap();
    v.flush().unwrap();
    assert_eq!(v.st.lock().free_now(), free - (MIB / 4096) as u64, "the unlinked file's blocks");
    v.sync_fs().unwrap();
    assert_clean(&s, &ld.snapshot(), "after an unlink with the data at the disk");
}

const WRITERS: usize = 6;
const FILE: usize = 24 * MIB;

/// Six programs, 24 MiB each, a megabyte per write, fsyncs in between, a seventh thread committing the journal
/// all the while. The data must go to the disk in large requests of its own (not through the disk's cache), and
/// every file must read back byte for byte: after a replay of the disk as it is (no clean unmount), and after a
/// clean remount.
#[test]
fn six_writers_of_large_files_are_bit_identical_after_a_replay_and_a_remount() {
    let s = Scratch::new();
    let (ld, dev, ns, v, _m) = disk_ns(&s, "256M", LAYOUT);
    let ns = Arc::new(ns);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let committer = {
        let (v, stop) = (v.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                v.flush().unwrap();
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };
    let writers: Vec<_> = (0..WRITERS)
        .map(|k| {
            let ns = ns.clone();
            std::thread::spawn(move || {
                let data = pattern(FILE, 100 + k as u32);
                let path = format!("/data/w{k}");
                let f = Ctx::kernel(&ns).open(None, path.as_bytes(), O_RDWR | O_CREAT, 0o644).unwrap();
                for (i, chunk) in data.chunks(MIB).enumerate() {
                    assert_eq!(f.pwrite((i * MIB) as u64, chunk), Ok(chunk.len()));
                    if i % 5 == k % 5 {
                        ns.lookup(&path).unwrap().inode.sync(SyncKind::Fsync).unwrap();
                    }
                }
                ns.lookup(&path).unwrap().inode.sync(SyncKind::Fsync).unwrap();
            })
        })
        .collect();
    writers.into_iter().for_each(|t| t.join().unwrap());
    stop.store(true, Ordering::Release);
    committer.join().unwrap();

    let total = (WRITERS * FILE) as u64;
    let big = ld.big_bytes.load(Ordering::Relaxed);
    assert!(big >= total * 9 / 10, "{} of {} MiB reached the device in large writes", big >> 20, total >> 20);
    assert!(dev.cached_pages() < 2048, "{} pages in the disk's cache after {} MiB of file data", dev.cached_pages(), total >> 20);
    let same = |v: &crate::Volume, what: &str| {
        for k in 0..WRITERS {
            let got = read_all(v, lookup(v, &format!("/w{k}")));
            assert!(got == pattern(FILE, 100 + k as u32), "{what}: w{k} differs ({} bytes)", got.len());
        }
    };
    // The disk as it is now: every fsync returned, the log not emptied.
    let (cut, v2) = mount(ld.snapshot());
    same(&v2, "after a replay");
    v2.sync_fs().unwrap();
    drop(v2);
    assert_clean(&s, &cut.image(), "six writers, replayed");
    v.sync_fs().unwrap();
    let img = ld.snapshot();
    assert_clean(&s, &img, "six writers");
    let (_d3, v3) = mount(img);
    same(&v3, "after a remount");
}
