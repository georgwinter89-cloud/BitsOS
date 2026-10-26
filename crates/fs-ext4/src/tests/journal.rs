//! FS-8b, the journal: dirty journals made by e2fsprogs (debugfs `jw`, with and without csum v3) are
//! replayed with their revokes; our own journal survives a cut after every single block write (the replayed
//! image is `e2fsck -fn` clean without repair and every fsync-ed file is complete), e2fsck replays our log
//! as well, a read-only mount replays into memory and writes nothing, freeze leaves a clean image, a freed
//! and reused metadata block is not overwritten by replay, hostile journals never panic.

use super::util::*;
use super::write::{file, put};
use crate::inode::ROOT;
use crate::{Kind, MemDev, Volume};
use std::format;
use std::process::Command;
use std::string::String;
use std::sync::Arc;
use std::vec;
use std::vec::Vec;

const RECOVER: u32 = crate::sb::incompat::RECOVER;

fn incompat(img: &[u8]) -> u32 {
    crate::le::g32(img, 1024 + crate::sb::FEATURE_INCOMPAT)
}

pub(super) fn ro() -> crate::Options {
    crate::Options { writable: false, now }
}

/// A file's bytes by path (None when it is not there or unreadable).
pub(super) fn content(v: &Volume, path: &str) -> Option<Vec<u8>> {
    let mut cur = ROOT;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        cur = v.list(cur).ok()?.into_iter().find(|(n, _, _)| n == part.as_bytes())?.1;
    }
    let st = v.stat(cur).ok()?;
    let mut b = vec![0u8; st.size() as usize];
    let n = v.read(cur, 0, &mut b).ok()?;
    b.truncate(n);
    Some(b)
}

pub(super) fn debugfs_w(s: &Scratch, img: &[u8], cmds: &str) -> Vec<u8> {
    let p = s.path("dirty.img");
    std::fs::write(&p, img).unwrap();
    let c = s.path("cmds");
    std::fs::write(&c, cmds).unwrap();
    let o = Command::new("debugfs").args(["-w", "-f"]).arg(&c).arg(&p).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    std::fs::read(&p).unwrap()
}

#[test]
fn linux_dirty_journals_are_replayed() {
    for jo in ["jo", "jo -c"] {
        let s = Scratch::new();
        let img = mkfs(&s, "32M", &["-b", "4096"], None);
        let mut data: Vec<u8> = (0..8192u32).map(|i| (i * 7 + 3) as u8).collect();
        // The second block starts with the jbd2 magic: debugfs logs it escaped.
        data[4096..4100].copy_from_slice(&crate::jbd2::MAGIC.to_be_bytes());
        let blk = s.path("blk");
        std::fs::write(&blk, &data).unwrap();
        let img = debugfs_w(&s, &img, &format!("{jo}\njw -b 7000,7001 {}\njw -r 7000 /dev/null\njw -b 7002 {}\njc\n", blk.display(), blk.display()));
        assert_ne!(incompat(&img) & RECOVER, 0, "{jo}: debugfs left the journal dirty");
        // Read-only: replayed in memory, nothing written.
        let dev = Arc::new(MemDev::new(img.clone()));
        let v = Volume::mount(dev.clone(), ro()).unwrap();
        let mut b = vec![0u8; 4096];
        v.dev.read(7001 * 4096, &mut b).unwrap();
        assert_eq!(&b[..], &data[4096..], "{jo}: escaped block not restored in memory");
        v.dev.read(7000 * 4096, &mut b).unwrap();
        assert!(b.iter().all(|&x| x == 0), "{jo}: a revoked block was replayed");
        assert_eq!(v.create(ROOT, b"x", 0o100644, 0, 0, Kind::Reg).err(), Some(crate::errno::EROFS));
        assert!(dev.image() == img, "{jo}: a read-only mount wrote");
        drop(v);
        // Writable: replayed to the disk, the log emptied, needs_recovery cleared.
        let (dev, v) = mount(img.clone());
        let out = dev.image();
        assert_eq!(&out[7001 * 4096..7002 * 4096], &data[4096..], "{jo}: block 7001");
        assert_eq!(&out[7002 * 4096..7003 * 4096], &data[..4096], "{jo}: block 7002 (after the revoke, not revoked)");
        assert!(out[7000 * 4096..7001 * 4096].iter().all(|&x| x == 0), "{jo}: block 7000 was revoked");
        let f = file(&v, ROOT, "after-replay");
        put(&v, f, 0, b"written after the replay");
        v.flush().unwrap();
        v.sync_fs().unwrap();
        let out = dev.image();
        assert_eq!(incompat(&out) & RECOVER, 0);
        assert_clean(&s, &out, &format!("{jo}: replay, a change, sync"));
        let dump = debugfs(&s, &out, "logdump");
        assert!(dump.contains("Journal starts at block 0") || !dump.contains("Found expected sequence"), "{jo}: log not empty:\n{dump}");
    }
}

/// The journal layouts of the cut tests: 4 KiB with metadata_csum (csum v3, 64bit) and 1 KiB 32-bit
/// without checksums.
fn jlayouts() -> [(&'static str, &'static [&'static str]); 2] {
    [("4k-v3", &["-b", "4096"]), ("1k-nocsum", &["-b", "1024", "-O", "^64bit,^metadata_csum"])]
}

/// A change sequence with fsyncs: returns (write-log length when each fsync returned, path, content).
fn fsynced_changes(v: &Volume, dev: &MemDev) -> Vec<(usize, String, Vec<u8>)> {
    let mut done = Vec::new();
    let d = v.create(ROOT, b"d", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let mut files = Vec::new();
    for i in 0..12usize {
        let f = file(v, d, &format!("f{i}"));
        let data = vec![i as u8 + 1; 3000 + i * 1500];
        put(v, f, 0, &data);
        files.push((f, data));
        if i % 3 == 2 {
            v.flush().unwrap();
            for (k, (_, c)) in files.iter().enumerate() {
                done.push((dev.writes() as usize, format!("/d/f{k}"), c.clone()));
            }
        }
    }
    v.remove(d, b"f0", files[0].0).unwrap();
    done.retain(|(_, p, _)| p != "/d/f0");
    v.rename(d, b"f1", files[1].0, ROOT, b"moved", None).unwrap();
    done.retain(|(_, p, _)| p != "/d/f1");
    v.set_size(files[4].0, 10).unwrap();
    done.retain(|(_, p, _)| p != "/d/f4");
    let held = files[5].0;
    v.hold(held).unwrap();
    v.remove(d, b"f5", held).unwrap();
    done.retain(|(_, p, _)| p != "/d/f5");
    let s = v.create(d, b"sub", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    v.create(s, b"link", 0o120777, 0, 0, Kind::Symlink("z".repeat(80).as_bytes())).unwrap();
    let late = file(v, s, "late");
    put(v, late, 0, &vec![0x5A; 9000]);
    v.flush().unwrap();
    done.push((dev.writes() as usize, "/moved".into(), files[1].1.clone()));
    done.push((dev.writes() as usize, "/d/sub/late".into(), vec![0x5A; 9000]));
    // Not fsync-ed: overwritten in place (any mix of old and new may survive a cut).
    done.retain(|(_, p, _)| p != "/d/f6");
    for k in 0..20u64 {
        put(v, files[6].0, k * 4096, b"overwritten");
    }
    done
}

/// The writes of a log as single blocks (a cut can fall inside a multi-block request), flushes dropped,
/// with the index of the request each came from.
fn block_writes(log: &crate::dev::WriteLog, bs: usize) -> Vec<(usize, u64, Vec<u8>)> {
    let mut out = Vec::new();
    for (i, (at, d)) in log.iter().filter(|(a, _)| *a != u64::MAX).enumerate() {
        for (k, c) in d.chunks(bs).enumerate() {
            out.push((i + 1, at + (k * bs) as u64, c.to_vec()));
        }
    }
    out
}

fn check_replayed(s: &Scratch, img: Vec<u8>, want: &[(usize, String, Vec<u8>)], req: usize, what: &str, e2fsck_too: bool) {
    if e2fsck_too {
        let (r1, r2, out) = repair(s, &img);
        assert!(r1 <= 1 && r2 == 0 && !out.contains("corrupt") && !out.contains("checksum"), "{what}: e2fsck's replay of our log: -fy {r1}, then -fn {r2}\n{out}");
    }
    let dev = Arc::new(MemDev::new(img));
    let v = Volume::mount(dev.clone(), opts()).unwrap_or_else(|e| panic!("{what}: mount {e}"));
    for (at, p, c) in want {
        if *at <= req {
            let got = content(&v, p);
            assert!(got.as_deref() == Some(&c[..]), "{what}: fsync-ed {p} (fsync returned after request {at}): {:?} bytes, want {}", got.map(|g| g.len()), c.len());
        }
    }
    v.sync_fs().unwrap();
    drop(v);
    let (c, out) = fsck(s, &dev.image());
    assert_eq!(c, 0, "{what}: e2fsck -fn after V2's replay\n{out}");
}

#[test]
fn every_cut_with_a_journal_is_clean_and_keeps_fsynced_files() {
    for (name, o) in jlayouts() {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "16M", o, None));
        v.sync_fs().unwrap();
        let before = dev.image();
        let bs = v.geo.bs as usize;
        dev.record();
        let base = dev.writes() as usize;
        let mut want = fsynced_changes(&v, &dev);
        for w in &mut want {
            w.0 -= base;
        }
        let log = dev.take_log();
        drop(v);
        let blocks = block_writes(&log, bs);
        assert!(blocks.len() > 100, "{name}: {} block writes", blocks.len());
        let replays = crate::journal::replays();
        let mut img = before.clone();
        for k in 0..=blocks.len() {
            if k > 0 {
                let (_, at, d) = &blocks[k - 1];
                img[*at as usize..*at as usize + d.len()].copy_from_slice(d);
            }
            // The fsync-ed state counts once every request up to its fsync is complete.
            let req = if k == blocks.len() { blocks.last().map_or(0, |b| b.0) } else { blocks[k].0 - 1 };
            check_replayed(&s, img.clone(), &want, req, &format!("{name}: cut after block write {k} of {}", blocks.len()), k % 16 == 0);
        }
        let n = crate::journal::replays() - replays;
        assert!(n as usize > blocks.len() / 3, "{name}: only {n} of {} cuts needed a replay", blocks.len());
    }
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// Each cut keeps everything before the last flush and a random half of the block writes after it: the
/// flushes around the commit block are what makes the journal safe on a disk with a volatile cache.
#[test]
fn reordered_cuts_with_a_journal_are_clean() {
    for (name, o) in jlayouts() {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "16M", o, None));
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        let base = dev.writes() as usize;
        let mut want = fsynced_changes(&v, &dev);
        for w in &mut want {
            w.0 -= base;
        }
        let log = dev.take_log();
        drop(v);
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let step = (log.len() / 60).max(1);
        for k in (0..log.len()).step_by(step).chain([log.len()]) {
            let flush = log[..k].iter().rposition(|(a, _)| *a == u64::MAX).map_or(0, |i| i + 1);
            let durable = log[..flush].iter().filter(|(a, _)| *a != u64::MAX).count();
            let mut img = before.clone();
            for (i, (at, d)) in log[..k].iter().enumerate() {
                if *at == u64::MAX {
                    continue;
                }
                for (c, chunk) in d.chunks(v_bs(o)).enumerate() {
                    if i >= flush && rng.next() & 1 == 0 {
                        continue;
                    }
                    let a = *at as usize + c * chunk.len();
                    img[a..a + chunk.len()].copy_from_slice(chunk);
                }
            }
            check_replayed(&s, img, &want, durable, &format!("{name}: cut at request {k} of {} (flush at {flush}), half of the rest", log.len()), k % (4 * step) == 0);
        }
    }
}

fn v_bs(o: &[&str]) -> usize {
    if o.contains(&"1024") { 1024 } else { 4096 }
}

/// A transaction committed in the log but not yet at its home places (cut right after the commit's
/// flush): e2fsck replays our log, a read-only mount shows it from memory, a writable mount replays it.
#[test]
fn a_committed_transaction_is_replayed_by_us_and_by_e2fsck() {
    for (name, o) in jlayouts() {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "16M", o, None));
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        let f = file(&v, ROOT, "committed");
        let data: Vec<u8> = (0..20_000u32).map(|i| (i % 253) as u8).collect();
        put(&v, f, 0, &data);
        v.flush().unwrap();
        let log = dev.take_log();
        let last = log.iter().rposition(|(a, _)| *a == u64::MAX).unwrap();
        let mut img = before.clone();
        for (at, d) in &log[..last] {
            if *at != u64::MAX {
                img[*at as usize..*at as usize + d.len()].copy_from_slice(d);
            }
        }
        assert_ne!(incompat(&img) & RECOVER, 0, "{name}: needs_recovery on disk");
        // e2fsck's replay.
        let (r1, r2, out) = repair(&s, &img);
        assert!(r1 <= 1 && r2 == 0, "{name}: e2fsck -fy {r1}, -fn {r2}\n{out}");
        let cat = Command::new("debugfs").args(["-R", "cat /committed"]).arg(s.path("repair.img")).output().unwrap();
        assert!(cat.stdout == data, "{name}: e2fsck's replay lost the file ({} bytes)", cat.stdout.len());
        // A read-only mount: in memory only.
        let rdev = Arc::new(MemDev::new(img.clone()));
        let rv = Volume::mount(rdev.clone(), ro()).unwrap();
        assert_eq!(content(&rv, "/committed").as_deref(), Some(&data[..]), "{name}: read-only replay");
        assert!(rdev.image() == img, "{name}: the read-only mount wrote");
        // A writable mount.
        let (wdev, wv) = mount(img);
        assert_eq!(content(&wv, "/committed").as_deref(), Some(&data[..]), "{name}: writable replay");
        wv.sync_fs().unwrap();
        assert_clean(&s, &wdev.image(), &format!("{name}: after our replay"));
    }
}

/// A directory's blocks (logged) are freed and taken by a file's data before the log is emptied: replay
/// must not write the logged directory blocks over the data (revoke records).
#[test]
fn freed_metadata_reused_as_data_survives_replay() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "8M", &["-b", "1024", "-O", "^metadata_csum,^64bit"], None));
    let d = v.create(ROOT, b"big", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let mut names = Vec::new();
    for i in 0..400 {
        let n = format!("a-long-name-to-fill-directory-blocks-{i:04}");
        let ino = file(&v, d, &n);
        names.push((n, ino));
    }
    v.flush().unwrap();
    let dblocks: Vec<u64> = {
        let st = v.lock();
        let di = st.inode(None, d).unwrap();
        (0..di.size() / 1024).filter_map(|l| st.map_at(None, &di, l).ok().and_then(|r| r.ok()).map(|r| r.0)).collect()
    };
    for (n, ino) in &names {
        v.remove(d, n.as_bytes(), *ino).unwrap();
    }
    v.remove(ROOT, b"big", d).unwrap();
    v.flush().unwrap();
    // Fill the volume so the freed directory blocks are taken by data.
    let mut k = 0;
    let mut reused = false;
    while !reused && k < 60 {
        let f = file(&v, ROOT, &format!("data{k}"));
        put(&v, f, 0, &vec![0xD0 + (k % 16) as u8; 64 * 1024]);
        v.flush().unwrap();
        let st = v.lock();
        let di = st.inode(None, f).unwrap();
        reused = (0..64).any(|l| st.map_at(None, &di, l).ok().and_then(|r| r.ok()).is_some_and(|r| dblocks.contains(&r.0)));
        k += 1;
    }
    assert!(reused, "no directory block was reused within {k} files");
    let img = dev.image();
    assert_ne!(incompat(&img) & RECOVER, 0);
    drop(v);
    let (dev2, v2) = mount(img);
    for i in 0..k {
        assert_eq!(content(&v2, &format!("/data{i}")), Some(vec![0xD0 + (i % 16) as u8; 64 * 1024]), "data{i} after replay");
    }
    v2.sync_fs().unwrap();
    assert_clean(&s, &dev2.image(), "reuse and replay");
}

/// Many fsyncs: the log fills, the tail moves, and cuts after that still replay clean.
#[test]
fn the_log_wraps() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "16M", &["-b", "1024", "-J", "size=1"], None));
    v.sync_fs().unwrap();
    let before = dev.image();
    dev.record();
    let base = dev.writes() as usize;
    let f = file(&v, ROOT, "log");
    let mut want = Vec::new();
    let mut body = Vec::new();
    let resets = crate::journal::tail_resets();
    for i in 0..400u32 {
        body.extend_from_slice(format!("line {i}\n").as_bytes());
        put(&v, f, 0, &body);
        let g = file(&v, ROOT, &format!("n{i}"));
        put(&v, g, 0, &[i as u8; 100]);
        v.flush().unwrap();
        want.push((dev.writes() as usize - base, format!("/n{i}"), vec![i as u8; 100]));
    }
    assert!(crate::journal::tail_resets() > resets, "the log never filled");
    let log = dev.take_log();
    drop(v);
    let writes: Vec<_> = log.iter().filter(|(a, _)| *a != u64::MAX).collect();
    let step = (writes.len() / 30).max(1);
    for k in (writes.len() / 2..writes.len()).step_by(step).chain([writes.len()]) {
        let mut img = before.clone();
        for (at, d) in &writes[..k] {
            img[*at as usize..*at as usize + d.len()].copy_from_slice(d);
        }
        check_replayed(&s, img, &want, k, &format!("wrapped log: cut after {k} of {}", writes.len()), k % (5 * step) == 0);
    }
}

/// Every name and file below `dir` read (errors ignored): never a panic.
fn read_tree(v: &Volume, dir: u32, depth: u32) {
    if depth > 6 {
        return;
    }
    let Ok(l) = v.list(dir) else { return };
    for (_, ino, ft) in l.into_iter().take(100) {
        if ft == crate::dir::FT_DIR {
            read_tree(v, ino, depth + 1);
        } else if let Ok(st) = v.stat(ino) {
            let mut b = vec![0u8; st.size().min(64 * 1024) as usize];
            let _ = v.read(ino, 0, &mut b);
        }
    }
}

#[test]
fn freeze_leaves_a_clean_image_and_holds_changes() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "16M", &[], None));
    let f = file(&v, ROOT, "before");
    put(&v, f, 0, b"written before the freeze");
    v.freeze().unwrap();
    let img = dev.image();
    assert_eq!(incompat(&img) & RECOVER, 0, "frozen image needs no recovery");
    assert_clean(&s, &img, "frozen image");
    let v2 = v.clone();
    let t = std::thread::spawn(move || v2.create(ROOT, b"during", 0o100644, 0, 0, Kind::Reg).map(|_| ()));
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!t.is_finished(), "a change went through a frozen volume");
    assert!(dev.image() == img, "the frozen volume wrote");
    v.thaw();
    t.join().unwrap().unwrap();
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "after the thaw");
    let (_d, v3) = mount(dev.image());
    assert!(content(&v3, "/during").is_some() && content(&v3, "/before").as_deref() == Some(&b"written before the freeze"[..]));
}

/// VS-2b CR 3 (INT-W12): fsync (with and without a running transaction) and syncfs end with the device's
/// `settle`; freeze, thaw and the background commit never call it (a snapshot's freeze would wait for itself).
#[test]
fn fsync_and_syncfs_settle_freeze_does_not() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "16M", &[], None));
    let base = dev.settles();
    let f = file(&v, ROOT, "a");
    put(&v, f, 0, b"one");
    v.flush().unwrap();
    assert_eq!(dev.settles(), base + 1, "fsync with a running transaction");
    v.flush().unwrap();
    assert_eq!(dev.settles(), base + 2, "fsync without one");
    v.sync_fs().unwrap();
    assert_eq!(dev.settles(), base + 3, "syncfs");
    put(&v, f, 0, b"two");
    vfs::writeback::Flush::flush(&*v, vfs::pagecache::SyncKind::Background).unwrap();
    v.freeze().unwrap();
    v.thaw();
    assert_eq!(dev.settles(), base + 3, "background commit, freeze and thaw never settle");
}

/// Bits flipped in the log of a dirty journal (and its superblock): mount, read, change; no panic, no hang.
#[test]
fn hostile_journals_never_panic() {
    for (name, o) in jlayouts() {
        let s = Scratch::new();
        let (dev, v) = mount(mkfs(&s, "16M", o, None));
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        let d = v.create(ROOT, b"h", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
        for i in 0..30 {
            let f = file(&v, d, &format!("x{i}"));
            put(&v, f, 0, &vec![i as u8; 2000]);
        }
        v.flush().unwrap();
        let log = dev.take_log();
        let last = log.iter().rposition(|(a, _)| *a == u64::MAX).unwrap();
        let mut img = before;
        for (at, dd) in &log[..last] {
            if *at != u64::MAX {
                img[*at as usize..*at as usize + dd.len()].copy_from_slice(dd);
            }
        }
        let jblocks: Vec<u64> = {
            let st = v.lock();
            let di = st.inode(None, 8).unwrap();
            (0..40).filter_map(|l| st.map_at(None, &di, l).ok().and_then(|r| r.ok()).map(|r| r.0)).collect()
        };
        drop(v);
        let bs = v_bs(o) as u64;
        let mut rng = Rng(0xDEAD_BEEF_1234_5678);
        for round in 0..150 {
            let mut im = img.clone();
            for _ in 0..1 + rng.next() % 8 {
                let b = jblocks[(rng.next() % jblocks.len() as u64) as usize];
                let at = (b * bs + rng.next() % if rng.next() % 3 == 0 { 64 } else { bs }) as usize;
                im[at] ^= 1 << (rng.next() % 8);
            }
            for writable in [true, false] {
                let o2 = if writable { opts() } else { ro() };
                let dv = Arc::new(MemDev::new(im.clone()));
                if let Ok(v) = Volume::mount(dv, o2) {
                    read_tree(&v, ROOT, 0);
                    let _ = v.create(ROOT, b"after", 0o100644, 0, 0, Kind::Reg);
                    let _ = v.sync_fs();
                }
            }
            let _ = (name, round);
        }
    }
}
