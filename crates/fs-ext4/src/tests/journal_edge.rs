//! FS-8b fixer: the journal's edge paths. A damaged or stale descriptor, journal features V2 accepts or
//! refuses, a device that fails inside a commit, a transaction bigger than the log, a write-back during a
//! freeze that does not end.

use super::journal::{content, debugfs_w, ro};
use super::util::*;
use super::write::{file, put};
use crate::errno::{EBUSY, EROFS};
use crate::inode::ROOT;
use crate::jbd2::{self, gb32, gb64, pb32};
use crate::le::g16;
use crate::sb::{self, Access};
use crate::{Kind, MemDev, Volume};
use std::format;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::vec;
use std::vec::Vec;

/// The volume blocks of the journal inode, in journal block order.
fn jmap(img: &[u8]) -> Vec<u64> {
    let v = Volume::mount(Arc::new(MemDev::new(img.to_vec())), ro()).unwrap();
    let st = v.lock();
    let di = st.inode(None, 8).unwrap();
    (0..di.size() / v.geo.bs).map(|l| st.map_at(None, &di, l).unwrap().unwrap().0).collect()
}

fn block(dev: &MemDev, blk: u64, bs: usize) -> Vec<u8> {
    let mut b = vec![0u8; bs];
    crate::dev::Dev::read(dev, blk * bs as u64, &mut b).unwrap();
    b
}

#[test]
fn a_damaged_descriptor_is_judged_by_its_commit_time() {
    let s = Scratch::new();
    let bs = 4096usize;
    let img = mkfs(&s, "32M", &["-b", "4096"], None);
    let data: Vec<u8> = (0..4096u32).map(|i| (i * 13 + 1) as u8).collect();
    let blk = s.path("blk");
    std::fs::write(&blk, &data).unwrap();
    let img = debugfs_w(&s, &img, &format!("jo -c\njw -b 7000 {b}\njc\njo\njw -b 7001 {b}\njc\n", b = blk.display()));
    let jm = jmap(&img);
    let at = |j: usize| jm[j] as usize * bs;
    let jsb = jbd2::Jsb::parse(&img[at(0)..at(0) + bs], bs as u32, jm.len() as u64).unwrap();
    let lay = jbd2::Layout { bs, csum: jsb.csum(), is64: jsb.is64(), seed: jsb.seed() };
    assert!(lay.csum == jbd2::Csum::V3, "debugfs jo -c makes csum v3");
    let find = |ty: u32, seq: u32| {
        (1..jm.len()).find(|&j| gb32(&img[at(j)..], 0) == jbd2::MAGIC && gb32(&img[at(j)..], 4) == ty && gb32(&img[at(j)..], 8) == seq).unwrap()
    };
    let (d2, c1, c2) = (find(jbd2::DESCRIPTOR, jsb.sequence + 1), find(jbd2::COMMIT, jsb.sequence), find(jbd2::COMMIT, jsb.sequence + 1));
    assert!(gb64(&img[at(c1)..], 0x30) > 0, "debugfs stamps its commits");
    let mut damaged = img.clone();
    // Unused tag space: the tags stay readable, the descriptor's checksum fails.
    damaged[at(d2) + bs - 8] ^= 0x40;
    // A newer commit after the bad descriptor: damage. Transaction 1 replayed, 2 not, the mount read-only.
    let (dev, v) = mount(damaged.clone());
    assert_eq!(v.create(ROOT, b"x", 0o100644, 0, 0, Kind::Reg).err(), Some(EROFS), "a damaged journal mounted writable");
    assert_eq!(block(&dev, 7000, bs), data, "transaction 1");
    assert!(block(&dev, 7001, bs).iter().all(|&x| x == 0), "the damaged transaction was replayed");
    drop(v);
    // An older commit after it: a stale log (Linux's lazy journal init), the scan ends quietly.
    let mut stale = damaged;
    let c = &mut stale[at(c2)..at(c2) + bs];
    c[0x30..0x38].copy_from_slice(&0u64.to_be_bytes());
    pb32(c, 0x10, 0);
    let sum = crate::crc::crc32c(lay.seed, c);
    pb32(c, 0x10, sum);
    let (dev, v) = mount(stale);
    assert!(v.writable(), "a stale log made the mount read-only: {:?}", v.access);
    assert_eq!(block(&dev, 7000, bs), data);
    assert!(block(&dev, 7001, bs).iter().all(|&x| x == 0));
    v.sync_fs().unwrap();
    drop(v);
    assert_clean(&s, &dev.image(), "stale log replayed");
}

#[test]
fn journal_features_v2_accepts_and_refuses() {
    let s = Scratch::new();
    // A fast_commit volume: writable (Linux too writes no fast commits until the journal has the feature).
    let (dev, v) = mount(mkfs(&s, "64M", &["-O", "fast_commit"], None));
    assert!(v.writable(), "fast_commit volume: {:?}", v.access);
    let f = file(&v, ROOT, "fc");
    put(&v, f, 0, b"written on a fast_commit volume");
    v.flush().unwrap();
    v.sync_fs().unwrap();
    drop(v);
    assert_clean(&s, &dev.image(), "fast_commit volume written by V2");
    // Dirty journals V2 cannot replay, and a journal below JBD2_MIN_JOURNAL_BLOCKS: read-only.
    // 1 KiB blocks: a 4096-block journal leaves room for a fast-commit area above the minimum.
    let base = mkfs(&s, "32M", &["-b", "1024", "-O", "^64bit,^metadata_csum"], None);
    let blk = s.path("blk");
    std::fs::write(&blk, vec![0x33u8; 1024]).unwrap();
    let dirty = debugfs_w(&s, &base, &format!("jo\njw -b 7000 {}\njc\n", blk.display()));
    let j0 = jmap(&dirty)[0] as usize * 1024;
    assert!(gb32(&dirty[j0..], 0x10) >= 2048, "journal length {}", gb32(&dirty[j0..], 0x10));
    assert_ne!(gb32(&dirty[j0..], 0x1C), 0, "s_start: the journal is dirty");
    assert_eq!(gb32(&dirty[j0..], 0x28) & (jbd2::INCOMPAT_CSUM_V2 | jbd2::INCOMPAT_CSUM_V3), 0, "no superblock checksum to reseal");
    let patched = |edits: &[(usize, u32)]| {
        let mut i = dirty.clone();
        for &(o, v) in edits {
            pb32(&mut i, j0 + o, v);
        }
        i
    };
    let inc = gb32(&dirty[j0..], 0x28);
    let cases = [
        (patched(&[(0x28, inc | jbd2::INCOMPAT_FAST_COMMIT), (0x54, 8)]), "fast-commit journal needs recovery"),
        (patched(&[(0x28, inc | jbd2::INCOMPAT_ASYNC_COMMIT)]), "async-commit journal needs recovery"),
        (patched(&[(0x10, 1000)]), "journal too short"),
    ];
    for (img, why) in cases {
        let dev = Arc::new(MemDev::new(img.clone()));
        let v = Volume::mount(dev.clone(), opts()).unwrap();
        assert!(matches!(v.access, Access::ReadOnly(r) if r == why), "{why}: {:?}", v.access);
        drop(v);
        assert!(dev.image() == img, "{why}: the refused journal was written");
    }
}

#[test]
fn a_device_that_fails_in_a_commit_leaves_a_replayable_log() {
    for o in [&["-b", "4096"][..], &["-b", "1024", "-O", "^64bit,^metadata_csum"][..]] {
        let s = Scratch::new();
        let (dev0, v0) = mount(mkfs(&s, "16M", o, None));
        let kept = file(&v0, ROOT, "kept");
        put(&v0, kept, 0, &[7u8; 5000]);
        v0.flush().unwrap();
        drop(v0);
        let base = dev0.image();
        let change = |v: &Volume| {
            let d = v.create(ROOT, b"d", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
            let f = file(v, d, "new");
            put(v, f, 0, &[9u8; 9000]);
            v.remove(ROOT, b"kept", kept).unwrap();
        };
        let (dev, v) = mount(base.clone());
        change(&v);
        let w0 = dev.writes();
        v.flush().unwrap();
        let n = dev.writes() - w0;
        assert!(n >= 4, "{o:?}: a commit of {n} writes");
        drop(v);
        for k in 0..n {
            let (dev, v) = mount(base.clone());
            change(&v);
            dev.stop_after(k);
            assert!(v.flush().is_err(), "{o:?}: write {k} of the commit failed, fsync answered Ok");
            assert_eq!(v.create(ROOT, b"after", 0o100644, 0, 0, Kind::Reg).err(), Some(EROFS), "{o:?}: still writable after a failed commit");
            drop(v);
            let (d2, v2) = mount(dev.image());
            let gone = content(&v2, "/kept").is_none();
            assert!(gone || content(&v2, "/kept").as_deref() == Some(&[7u8; 5000][..]), "{o:?} k={k}: the committed file is damaged");
            v2.sync_fs().unwrap();
            drop(v2);
            assert_clean(&s, &d2.image(), &format!("{o:?}: commit failed at write {k} of {n}, then replay"));
        }
    }
}

#[test]
fn a_transaction_bigger_than_the_log_is_marked_for_e2fsck_while_written() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "16M", &["-b", "1024", "-O", "^64bit,^metadata_csum"], None));
    v.sync_fs().unwrap();
    let before = dev.image();
    v.lock().jnl.as_mut().unwrap().test_cap = 4;
    let around = crate::journal::too_big();
    dev.record();
    let d = v.create(ROOT, b"big", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    for i in 0..4u8 {
        let f = file(&v, d, &format!("f{i}"));
        put(&v, f, 0, &vec![i + 1; 20000]);
    }
    v.remove(d, b"f1", v.list(d).unwrap().into_iter().find(|e| e.0 == b"f1").unwrap().1).unwrap();
    v.flush().unwrap();
    let log = dev.take_log();
    assert!(crate::journal::too_big() > around, "no transaction went around the log");
    v.sync_fs().unwrap();
    drop(v);
    let img = dev.image();
    assert_eq!(g16(&img, 1024 + sb::STATE) & sb::STATE_ERROR, 0, "the error mark stayed");
    assert_clean(&s, &img, "transactions written around the log");
    let writes: Vec<_> = log.iter().filter(|(a, _)| *a != u64::MAX).collect();
    let mut marked = 0;
    for k in 0..=writes.len() {
        let mut im = before.clone();
        for (at, d) in &writes[..k] {
            im[*at as usize..*at as usize + d.len()].copy_from_slice(d);
        }
        if g16(&im, 1024 + sb::STATE) & sb::STATE_ERROR != 0 {
            marked += 1;
            let v = Volume::mount(Arc::new(MemDev::new(im.clone())), opts()).unwrap();
            assert!(!v.writable(), "cut {k}: a volume marked for e2fsck mounted writable");
            drop(v);
            let (r1, r2, out) = repair(&s, &im);
            assert!(r1 <= 2 && r2 == 0, "cut {k} inside a write-around: e2fsck -fy {r1}, then -fn {r2}\n{out}");
        } else {
            let (d2, v2) = mount(im);
            v2.sync_fs().unwrap();
            drop(v2);
            let (c, out) = fsck(&s, &d2.image());
            assert_eq!(c, 0, "cut {k} of {} outside a write-around\n{out}", writes.len());
        }
    }
    assert!(marked > 0, "no cut fell inside a write-around");
}

#[test]
fn a_write_back_waits_at_most_ten_seconds_for_a_thaw() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "16M", &[], None));
    let f = file(&v, ROOT, "wb");
    v.freeze().unwrap();
    let t = Instant::now();
    assert_eq!(v.write(f, 0, &[1u8; 4096], 4096, 0..1).err(), Some(EBUSY));
    assert!(t.elapsed() >= Duration::from_secs(9), "gave up after {:?}", t.elapsed());
    let t = Instant::now();
    assert_eq!(v.write(f, 0, &[1u8; 4096], 4096, 0..1).err(), Some(EBUSY));
    assert!(t.elapsed() < Duration::from_secs(1), "the second write-back waited again");
    v.thaw();
    v.write(f, 0, &[1u8; 4096], 4096, 0..1).unwrap();
    v.sync_fs().unwrap();
    drop(v);
    assert_clean(&s, &dev.image(), "write-back after the thaw");
}
