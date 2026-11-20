//! FSR: a truncate to a size inside a block zeroes the rest of that block straight on the disk (FS-W2's data path,
//! `State::set_file_size`), not into the disk's cache: the block is on the disk when the call returns, and the next
//! flush writes nothing for it. Ordered like a step's data: at every cut where the new size is on the disk, so are
//! the zeros (with a journal: any piece the device took after its last flush may be lost; without one: the prefix).

use super::disk::*;
use super::flight::{pattern, read_all, step};
use super::lay::{cut_image, Rng};
use super::util::*;
use super::write::file;
use crate::dev::{is_mark, Dev};
use crate::inode::ROOT;
use crate::Volume;
use std::format;
use std::string::String;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::vec::Vec;

const B: usize = 4096;
const CUT: usize = 2 * B + 100;

fn tail_is_zero(img: &[u8], p: u64) -> bool {
    img[p as usize * B + CUT % B..(p as usize + 1) * B].iter().all(|&x| x == 0)
}

fn run(layout: &[&str], journal: bool) -> Vec<String> {
    let s = Scratch::new();
    let (ld, dev, v) = disk_volume(&s, "32M", layout);
    let f = file(&v, ROOT, "f");
    let data = pattern(3 * B, 9);
    step(&v, f, 0, &data, 3 * B as u64).unwrap();
    v.sync_fs().unwrap();
    let p = blocks_of(&v, f)[2];
    let before = ld.snapshot();
    let cached = dev.cached_pages();
    ld.record();
    v.set_size(f, CUT as u64).unwrap();
    assert!(dev.cached_pages() <= cached + 2, "{layout:?}: the edge block went through the disk's cache");
    assert!(tail_is_zero(&ld.snapshot(), p), "{layout:?}: the zeros are not on the disk when the truncate returns");
    let at_size = ld.log_len();
    v.flush().unwrap();
    let log = ld.take_log();
    assert!(log[at_size..].iter().all(|(a, _)| is_mark(*a) || *a / B as u64 != p), "{layout:?}: the flush wrote the edge block again (it sat dirty in the disk's cache)");
    v.set_size(f, 3 * B as u64).unwrap();
    let mut want = data[..CUT].to_vec();
    want.resize(3 * B, 0);
    assert_eq!(read_all(&v, f), want, "{layout:?}: growing again reads zeros past the cut");
    drop(v);

    let mut bad = Vec::new();
    let mut rng = Rng(0x0F5A_11CE_D00D_2026);
    let rounds = if journal { 4 } else { 1 };
    for k in (0..=log.len()).flat_map(|k| std::vec![k; rounds]) {
        let mut kept = || rng.bit();
        let (img, flush) = cut_image(&before, &log, k, B, if journal { Some(&mut kept) } else { None });
        let what = format!("{layout:?}: cut after {k} of {} device calls (last flush at {flush})", log.len());
        let dev = std::sync::Arc::new(crate::MemDev::new(img));
        let vol = match Volume::mount(dev.clone(), opts()) {
            Ok(v) => v,
            Err(e) => {
                bad.push(format!("{what}: mount errno {e}"));
                continue;
            }
        };
        let got = read_all(&vol, f);
        if got.len() == CUT && !tail_is_zero(&dev.image(), p) {
            bad.push(format!("{what}: the new size is on the disk, the zeros past it are not"));
        }
        // Before the size commits, the old size may show the zeros already (as on Linux, and as when the disk's
        // cache wrote them at the commit's first flush); never anything else.
        let tail_ok = got.len() <= CUT || got[CUT..] == data[CUT..got.len()] || got[CUT..].iter().all(|&x| x == 0);
        if got.len() > data.len() || got[..got.len().min(CUT)] != data[..got.len().min(CUT)] || !tail_ok {
            bad.push(format!("{what}: {} bytes, not the file's", got.len()));
        }
    }
    bad
}

#[test]
fn a_truncate_inside_a_block_zeroes_it_on_the_disk_before_the_size_commits() {
    let bad = run(&["-b", "4096"], true);
    assert!(bad.is_empty(), "{} bad cuts, first: {:?}", bad.len(), bad.first());
}

#[test]
fn without_a_journal_the_zeros_come_before_the_size_at_every_prefix() {
    let bad = run(&["-b", "4096", "-O", "^has_journal"], false);
    assert!(bad.is_empty(), "{} bad cuts, first: {:?}", bad.len(), bad.first());
}

/// The truncate holds `ext4.vol`: its edge block must not wait for the disk cache's write-back lock, which a flush
/// holds while it writes the log and the metadata (here: one page held at the device). Read through the cache, the
/// block's page would be cached and `write_direct` would wait for that lock (its slow path).
#[test]
fn a_truncate_does_not_wait_for_a_flush_writing_the_cache() {
    for layout in [&["-b", "4096"][..], &["-b", "4096", "-O", "^has_journal"][..]] {
        let s = Scratch::new();
        let (ld, dev, v) = disk_volume(&s, "32M", layout);
        let _open = Open(ld.clone());
        let f = file(&v, ROOT, "f");
        step(&v, f, 0, &pattern(3 * B, 9), 3 * B as u64).unwrap();
        v.sync_fs().unwrap();
        // A free block's page, dirty in the disk's cache; the flush's write of it waits at the device.
        let x = 8000 * B as u64;
        dev.write(x, &[0x77; B]).unwrap();
        ld.hold_next(x, x + B as u64);
        let d = dev.clone();
        let flush = std::thread::spawn(move || d.flush());
        wait("the flush's write at the device", || ld.held.load(Ordering::Acquire) == 1);
        let w = v.clone();
        let cut = std::thread::spawn(move || w.set_size(f, CUT as u64));
        std::thread::sleep(Duration::from_millis(100));
        let early = cut.is_finished();
        ld.hold_shut.store(false, Ordering::Release);
        assert_eq!(flush.join().unwrap(), Ok(()));
        assert_eq!(cut.join().unwrap(), Ok(()));
        assert!(early, "{layout:?}: the truncate waited for a flush's write-out under the volume lock");
    }
}

/// Fixer round (checker finding 1): the edge block's write may time out at the device and still land later. The
/// truncate answers EIO (not ETIMEDOUT) and the block is kept out of use in this mount: after the file is gone and
/// the commit is through, the volume is filled to the last block and no write of anything (data, directory, extent
/// tree, log) goes to the block the late write may hit.
#[test]
fn a_timed_out_edge_block_is_kept_out_of_use_until_the_next_mount() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "32M", &["-b", "4096"]);
    let f = file(&v, ROOT, "f");
    step(&v, f, 0, &pattern(3 * B, 9), 3 * B as u64).unwrap();
    v.sync_fs().unwrap();
    let p = blocks_of(&v, f)[2];
    ld.time_out_next(p * B as u64, (p + 1) * B as u64);
    assert_eq!(v.set_size(f, CUT as u64), Err(crate::errno::EIO), "a timed-out truncate answers EIO to the program");
    assert!(v.writable(), "a timeout stopped the volume");
    assert!(v.st.lock().flights.late_in(p, p + 1).next().is_some(), "the edge block is not kept out of use");
    ld.record();
    v.remove(ROOT, b"f", f).unwrap();
    v.flush().unwrap();
    // Megabyte-sized pieces, then single blocks, each in a file of its own directory now and then, until the
    // allocator has nothing left to give.
    let mut full = 0;
    for (i, len) in [256 * 1024, B].into_iter().enumerate() {
        let data = pattern(len, 3);
        'fill: for n in 0.. {
            let d = v.create(ROOT, format!("d{i}.{n}").as_bytes(), 0o40755, 0, 0, crate::Kind::Dir);
            let Ok(d) = d else { break };
            for j in 0..32u64 {
                let g = file(&v, d.ino, &format!("g{j}"));
                if let Err(e) = step(&v, g, 0, &data, len as u64) {
                    assert_eq!(e, crate::errno::ENOSPC, "filling the volume");
                    full += 1;
                    break 'fill;
                }
            }
            v.flush().unwrap();
        }
    }
    v.sync_fs().unwrap();
    assert_eq!(full, 2, "the volume was not filled");
    let (lo, hi) = (p * B as u64, (p + 1) * B as u64);
    let hit = ld.take_log().iter().filter(|(a, _)| !is_mark(*a)).any(|(a, d)| *a < hi && a + d.len() as u64 > lo);
    assert!(!hit, "the timed-out edge block was written again in this mount (handed to a file or to metadata)");
    assert_clean(&s, &ld.snapshot(), "after a timed-out truncate and a full volume");
}
