//! FS-W2 (the checker's probe, taken over): on a full volume on the disk path five files are deleted one by one
//! and a new file's data, written straight to the disk, takes exactly their blocks; a cut at every device call.
//! A deleted file that is still there after the cut shows only its own bytes, the new file only bytes written to
//! it. With a journal the unflushed writes are lost at random and e2fsck must find nothing (a freed block is busy
//! until the commit that frees it, `journal`). Without one a freed block waits for the device flush that makes its
//! free durable (`State::held`): a plain prefix cut, as `flight_cut` cuts that layout.
use super::disk::*;
use super::flight::{pattern, read_all, step, MIB};
use super::lay::{cut_image, Rng};
use super::util::*;
use super::write::file;
use crate::errno::ENOSPC;
use crate::inode::ROOT;
use crate::Volume;
use std::collections::BTreeSet;
use std::format;
use std::vec;
use std::vec::Vec;

const N: usize = 5;
const OLD_BYTES: usize = 2 * MIB;
const CH: usize = 256 << 10;

fn fill(v: &Volume, name: &str) -> u32 {
    let f = file(v, ROOT, name);
    let junk = vec![0x5Au8; MIB];
    let mut at = 0u64;
    for piece in [MIB, 64 << 10, 4096] {
        loop {
            match step(v, f, at, &junk[..piece], at + piece as u64) {
                Ok(()) => at += piece as u64,
                Err(ENOSPC) => break,
                Err(e) => panic!("filling the volume: errno {e}"),
            }
        }
    }
    f
}

fn probe(name: &str, layout: &[&str], journal: bool, sync_between: bool) -> Vec<std::string::String> {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "32M", layout);
    let mut olds = Vec::new();
    let mut old_blocks: BTreeSet<u64> = BTreeSet::new();
    for i in 0..N {
        let f = file(&v, ROOT, &format!("old{i}"));
        let d = pattern(OLD_BYTES, 100 + i as u32);
        for at in (0..OLD_BYTES).step_by(MIB) {
            step(&v, f, at as u64, &d[at..at + MIB], OLD_BYTES as u64).unwrap();
        }
        olds.push((f, d));
    }
    let newf = file(&v, ROOT, "new");
    fill(&v, "fill");
    v.sync_fs().unwrap();
    for (f, _) in &olds {
        old_blocks.extend(blocks_of(&v, *f));
    }
    let before = ld.snapshot();
    ld.record();
    let want = pattern(N * OLD_BYTES, 7);
    let mut len = 0usize;
    for i in 0..N {
        v.remove(ROOT, format!("old{i}").as_bytes(), olds[i].0).unwrap();
        if sync_between {
            v.flush().unwrap();
        }
        while len < (i + 1) * OLD_BYTES {
            match step(&v, newf, len as u64, &want[len..len + CH], (len + CH) as u64) {
                Ok(()) => len += CH,
                Err(ENOSPC) => break,
                Err(e) => panic!("{name}: errno {e}"),
            }
        }
        if i % 2 == 1 {
            v.flush().unwrap();
        }
    }
    v.flush().unwrap();
    let log = ld.take_log();
    let got: BTreeSet<u64> = blocks_of(&v, newf).into_iter().collect();
    let reused = got.intersection(&old_blocks).count();
    std::eprintln!("{name}: new file {len} bytes, {reused} of its {} blocks were old files' blocks, {} device calls", got.len(), log.len());
    assert!(reused >= 256, "{name}: only {reused} blocks reused");
    drop(v);

    let mut bad = Vec::new();
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let rounds = if journal { 3 } else { 1 };
    for k in (0..=log.len()).flat_map(|k| vec![k; rounds]) {
        let mut kept = || rng.bit();
        let (img, flush) = cut_image(&before, &log, k, 4096, if journal { Some(&mut kept) } else { None });
        let what = format!("{name}: cut after {k} of {} device calls (last flush at {flush})", log.len());
        let dev = std::sync::Arc::new(crate::MemDev::new(img));
        let vol = match Volume::mount(dev.clone(), opts()) {
            Ok(v) => v,
            Err(e) => {
                bad.push(format!("{what}: mount errno {e}"));
                continue;
            }
        };
        let names = vol.list(ROOT).unwrap();
        for (i, (_, d)) in olds.iter().enumerate() {
            let n = format!("old{i}");
            let Some(ino) = names.iter().find(|e| e.0 == n.as_bytes()).map(|e| e.1) else { continue };
            let got = read_all(&vol, ino);
            if got.len() > d.len() || got != d[..got.len()] {
                let blk = got.chunks(4096).zip(d.chunks(4096)).position(|(a, b)| a != b).unwrap_or(0);
                let news = got.chunks(4096).filter(|c| want.chunks(4096).any(|w| w == *c)).count();
                bad.push(format!("{what}: {n} is still there ({} bytes) and block {blk} is not its own; {news} of its blocks hold the NEW file's bytes", got.len()));
            }
        }
        let ino = names.iter().find(|e| e.0 == b"new").map(|e| e.1).unwrap();
        let got = read_all(&vol, ino);
        if got.len() > len || got != want[..got.len()] {
            bad.push(format!("{what}: new ({} bytes) holds bytes not written to it", got.len()));
        }
        if journal {
            vol.sync_fs().unwrap();
            drop(vol);
            let (code, out) = fsck(&s, &dev.image());
            if code != 0 {
                bad.push(format!("{what}: e2fsck {code}: {}", out.lines().take(4).collect::<Vec<_>>().join(" | ")));
            }
        }
    }
    bad
}

#[test]
fn blocks_of_deleted_files_reused_by_direct_writes_under_a_cut_with_a_journal() {
    for sync_between in [false, true] {
        let bad = probe("journal", &["-b", "4096", "-m", "0"], true, sync_between);
        assert!(bad.is_empty(), "sync_between={sync_between}: {} bad cuts, first:\n{}", bad.len(), bad.iter().take(5).cloned().collect::<Vec<_>>().join("\n"));
    }
}

#[test]
fn blocks_of_deleted_files_reused_by_direct_writes_under_a_prefix_cut_without_a_journal() {
    let bad = probe("bare", &["-b", "4096", "-m", "0", "-O", "^has_journal"], false, false);
    assert!(bad.is_empty(), "{} bad cuts, first:\n{}", bad.len(), bad.iter().take(5).cloned().collect::<Vec<_>>().join("\n"));
}

/// Without a journal a freed block is handed out only after a device flush made its free durable (`held`); the
/// background pass is such a flush when nobody syncs. With a journal nothing is held: the journal's busy list does it.
#[test]
fn without_a_journal_a_freed_block_waits_for_a_device_flush() {
    use std::sync::atomic::Ordering;
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "32M", &["-b", "4096", "-O", "^has_journal"]);
    let d = pattern(MIB, 3);
    let old = file(&v, ROOT, "old");
    step(&v, old, 0, &d, MIB as u64).unwrap();
    v.sync_fs().unwrap();
    let had: BTreeSet<u64> = blocks_of(&v, old).into_iter().collect();
    v.remove(ROOT, b"old", old).unwrap();
    let new = file(&v, ROOT, "new");
    step(&v, new, 0, &d, MIB as u64).unwrap();
    assert!(blocks_of(&v, new).iter().all(|b| !had.contains(b)), "a deleted file's blocks were written again before the delete was on the disk");
    assert!(!v.st.lock().held.is_empty());
    let flushes = ld.flushes.load(Ordering::Relaxed);
    vfs::writeback::Flush::flush(&*v, vfs::pagecache::SyncKind::Background).unwrap();
    assert!(ld.flushes.load(Ordering::Relaxed) > flushes, "the background pass did not flush the device for the held blocks");
    assert!(v.st.lock().held.is_empty(), "the flush did not let the blocks go");
    // A pass with nothing held costs no device flush.
    let flushes = ld.flushes.load(Ordering::Relaxed);
    vfs::writeback::Flush::flush(&*v, vfs::pagecache::SyncKind::Background).unwrap();
    assert_eq!(ld.flushes.load(Ordering::Relaxed), flushes);
    v.sync_fs().unwrap();
    assert_clean(&s, &ld.snapshot(), "after a delete and a write without a journal");

    let (_ld, _dev, v) = disk_volume(&s, "32M", &["-b", "4096"]);
    let old = file(&v, ROOT, "old");
    step(&v, old, 0, &d, MIB as u64).unwrap();
    v.remove(ROOT, b"old", old).unwrap();
    assert!(v.st.lock().held.is_empty(), "a volume with a journal holds freed blocks twice");
}
