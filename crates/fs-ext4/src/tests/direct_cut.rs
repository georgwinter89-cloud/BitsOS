//! FS-W2: the power cut at every device write of parallel writers on the disk path (`disk`: ext4's device on a
//! `block::Disk` over a recording device with a volatile cache). The writers go through the VFS, so their
//! write-back sends the files' pages themselves to the disk (`Dev::write_pages`); metadata and the log reach it
//! through the disk's cache at each commit's flush. Of what the device took after its last flush, each 4 KiB piece
//! is kept or lost at random (`lay::cut_image`: lost and reordered).
//!
//! Checked at every cut: the volume mounts (replay), every file shows under its size only bytes the run wrote to
//! it at that place, or a hole's zeros in the file two writers share (every free block held another file's bytes
//! before the run: a mapping that came before its data would show them); a file whose fsync returned before the
//! cut has everything written before that fsync; e2fsck finds nothing after the replay.
//!
//! Fixer round: the same run on a volume without a journal, for what such a volume promises on a disk: e2fsck can
//! repair every cut, the plain prefix and the lossy one. Nothing about the files' bytes is checked there. The
//! disk's cache writes the metadata of a flush in page order, not in the order of the changes (an inode can reach
//! the disk before the extent block it points at; with the first form of this test, which checked the bytes, a
//! prefix cut answered EUCLEAN for such a file). That is older than FS-W2 and Linux's guarantee without a journal.
//! What FS-W2 must not do is widen it: `direct_reuse_cut` and `held`.

use super::cut::check_disk;
use super::disk::*;
use super::flight::{pattern, read_all};
use super::flight_cut::{leave_old_bytes, OLD};
use super::lay::{cut_image, Rng};
use super::util::*;
use std::format;
use std::sync::{Arc, Mutex};
use std::vec::Vec;
use vfs::abi::{O_CREAT, O_RDWR};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

const CHUNK: usize = 256 << 10;
/// (name, bytes, writers)
const FILES: [(&str, usize, usize); 3] = [("a", 6 * CHUNK + 100, 1), ("b", 5 * CHUNK, 1), ("shared", 8 * CHUNK + 5000, 2)];

fn want(k: usize) -> Vec<u8> {
    pattern(FILES[k].1, 60 + k as u32)
}

/// An fsync that returned: (file, the device's log length then, the bytes of the file written before it).
type Synced = Arc<Mutex<Vec<(usize, usize, usize)>>>;

fn run(ns: &Arc<vfs::MountNs>, ld: &Arc<LogDisk>, synced: &Synced) {
    let mut threads = Vec::new();
    for (k, &(name, _, writers)) in FILES.iter().enumerate() {
        for t in 0..writers {
            let (ns, ld, synced) = (ns.clone(), ld.clone(), synced.clone());
            threads.push(std::thread::spawn(move || {
                let data = want(k);
                let path = format!("/data/{name}");
                let f = Ctx::kernel(&ns).open(None, path.as_bytes(), O_RDWR, 0).unwrap();
                let chunks = data.len().div_ceil(CHUNK);
                for i in (t..chunks).step_by(writers) {
                    let at = i * CHUNK;
                    let end = (at + CHUNK).min(data.len());
                    assert_eq!(f.pwrite(at as u64, &data[at..end]), Ok(end - at));
                    if i % 2 == 1 || i + writers >= chunks {
                        ns.lookup(&path).unwrap().inode.sync(SyncKind::Fsync).unwrap();
                        if writers == 1 {
                            synced.lock().unwrap().push((k, ld.log_len(), end));
                        }
                    }
                }
            }));
        }
    }
    threads.into_iter().for_each(|t| t.join().unwrap());
}

#[test]
fn a_cut_at_every_device_write_of_parallel_writers_on_a_disk() {
    every_cut("journal", &["-b", "4096"], true);
}

#[test]
fn a_cut_at_every_device_write_of_parallel_writers_on_a_disk_without_a_journal() {
    every_cut("bare", &["-b", "4096", "-O", "^has_journal"], false);
}

fn every_cut(name: &str, layout: &[&str], journal: bool) {
    let s = Scratch::new();
    let (ld, dev, ns, v, _m) = disk_ns(&s, "32M", layout);
    let ns = Arc::new(ns);
    leave_old_bytes(&v);
    for f in FILES {
        drop(Ctx::kernel(&ns).open(None, format!("/data/{}", f.0).as_bytes(), O_RDWR | O_CREAT, 0o644).unwrap());
    }
    v.sync_fs().unwrap();
    let before = ld.snapshot();
    let big0 = ld.big_bytes.load(std::sync::atomic::Ordering::Relaxed);
    ld.record();
    let synced: Synced = Arc::new(Mutex::new(Vec::new()));
    run(&ns, &ld, &synced);
    let log = ld.take_log();
    let synced = synced.lock().unwrap().clone();
    // The run's data went to the disk as its own requests, beside the disk's cache, into blocks that held old bytes.
    assert!(ld.big_bytes.load(std::sync::atomic::Ordering::Relaxed) - big0 >= 2 << 20, "the files' data did not reach the device in large writes");
    assert!(dev.cached_pages() < 256, "{} pages in the disk's cache", dev.cached_pages());
    let ino = lookup(&v, "/a");
    let first = blocks_of(&v, ino)[0] as usize;
    assert_eq!(before[first * 4096..first * 4096 + 4096], [OLD; 4096], "the run's first block was never used before");
    assert!(synced.len() >= 6, "fsyncs noted: {synced:?}");
    let (calls, flushes, large) = (log.len(), log.iter().filter(|e| e.0 == crate::dev::FLUSHED).count(), log.iter().filter(|e| e.1.len() >= BIG).count());
    assert!(flushes >= 6 && large >= 6, "{calls} device calls, {flushes} flushes, {large} large writes: too few to show anything");

    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut wholes = 0;
    if !journal {
        for k in 0..=log.len() {
            let (img, flush) = cut_image(&before, &log, k, 4096, None);
            check_disk(&s, img, &format!("{name}: cut after {k} of {} device calls (last flush at {flush})", log.len()));
            if k % 4 == 0 {
                let (img, flush) = cut_image(&before, &log, k, 4096, Some(&mut || rng.bit()));
                check_disk(&s, img, &format!("{name}: cut after {k} of {} device calls (last flush at {flush}), half of the rest lost", log.len()));
            }
        }
        return;
    }
    // Each cut four times: which pieces the device's cache lost matters as much as where the cut falls.
    for k in (0..=log.len()).flat_map(|k| [k; 4]) {
        let mut kept = || rng.bit();
        let (img, flush) = cut_image(&before, &log, k, 4096, Some(&mut kept));
        let what = format!("{name}: cut after {k} of {} device calls (last flush at {flush})", log.len());
        let (cut, vol) = mount(img);
        let mut whole = true;
        for (f, &(name, len, writers)) in FILES.iter().enumerate() {
            let got = read_all(&vol, lookup(&vol, &format!("/{name}")));
            let data = want(f);
            assert!(got.len() <= len, "{what}: {name} is {} bytes", got.len());
            whole &= got.len() == len && got == data;
            for (i, blk) in got.chunks(4096).enumerate() {
                let same = blk == &data[i * 4096..i * 4096 + blk.len()];
                let hole = writers > 1 && blk.iter().all(|&b| b == 0);
                assert!(same || hole, "{what}: block {i} of {name} (size {}) is not the run's data{}", got.len(), if blk.contains(&OLD) { ": it holds another file's old bytes" } else { "" });
            }
            // Ordered mode's other half: what an fsync answered for is there, whatever the device's cache lost.
            for &(_, at, bytes) in synced.iter().filter(|e| e.0 == f && e.1 <= k) {
                assert!(got.len() >= bytes && got[..bytes] == data[..bytes], "{what}: {name} lost bytes its fsync (at call {at}) had answered for: {} of {bytes}", got.len());
            }
        }
        wholes += whole as u32;
        vol.sync_fs().unwrap();
        drop(vol);
        assert_clean(&s, &cut.image(), &what);
    }
    assert!(wholes >= 1, "{name}: no cut showed the whole run");
}
