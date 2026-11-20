//! FSR (the desk's point from BIGDIR): `empty_journal` writes "log empty" to the journal superblock. When that
//! write fails after it was sent (here: the disk cache's write-out of it times out, its page stays dirty and goes
//! out with the next flush), the disk may say "empty" while the volume goes on committing into a log in use. The
//! next commit must write the superblock with its own start, so a cut before its home writes is still replayed.

use super::disk::*;
use super::flight::{pattern, read_all, step};
use super::util::*;
use super::write::file;
use crate::inode::ROOT;
use crate::jbd2::gb32;
use crate::{MemDev, Volume};
use std::sync::Arc;

/// `s_start` of the journal superblock (big-endian, byte 28).
fn s_start(img: &[u8], p0: u64) -> u32 {
    gb32(&img[p0 as usize * 4096..], 28)
}

#[test]
fn a_failed_log_empty_mark_does_not_hide_the_next_commit_from_a_replay() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "32M", &["-b", "4096"]);
    let f = file(&v, ROOT, "f");
    step(&v, f, 0, &pattern(8192, 1), 8192).unwrap();
    v.flush().unwrap();
    let p0 = v.lock().jnl.as_ref().unwrap().map.phys(0).unwrap();
    assert_ne!(s_start(&ld.snapshot(), p0), 0, "the log is in use after a commit");
    // syncfs: the log is emptied; its superblock's write-out times out (the volume stays writable: VS-H CR 5).
    ld.time_out_next(p0 * 4096, p0 * 4096 + 4096);
    assert!(v.sync_fs().is_err(), "the timed-out write of the log's superblock was not reported");
    assert!(v.writable());
    // A change and its commit. The disk now holds what a cut before the home writes leaves behind.
    let g = file(&v, ROOT, "g");
    step(&v, g, 0, &pattern(4096, 2), 4096).unwrap();
    v.flush().unwrap();
    let img = ld.snapshot();
    assert_ne!(s_start(&img, p0), 0, "the disk says the log is empty while it holds a committed transaction");
    drop(v);
    let w = Volume::mount(Arc::new(MemDev::new(img)), opts()).unwrap();
    let names = w.list(ROOT).unwrap();
    let gi = names.iter().find(|e| e.0 == b"g").map(|e| e.1);
    assert!(gi.is_some(), "the replay skipped the commit of g");
    assert_eq!(read_all(&w, gi.unwrap()), pattern(4096, 2));
}
