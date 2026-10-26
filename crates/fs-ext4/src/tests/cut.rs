//! Cut power: every k-th prefix of the device writes of a change sequence is laid over the image as it was
//! before; each such disk must mount (orphan cleanup runs), read without a panic, and be repaired by
//! `e2fsck -fy` to a state `e2fsck -fn` finds clean: never worse than a journal-less ext4 on Linux.
//! A second model stops the device after k writes (every later write fails): the volume turns read-only
//! and the image is as repairable.

use super::util::*;
use super::write::{file, put};
use crate::inode::ROOT;
use crate::{Kind, MemDev, Volume};
use std::format;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;

fn changes(v: &Volume) {
    let d = v.create(ROOT, b"c", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let mut files = Vec::new();
    for i in 0..40 {
        let f = file(v, d, &format!("f{i}"));
        put(v, f, 0, &std::vec![i as u8; 5000 + i * 700]);
        files.push(f);
    }
    for i in (0..40).step_by(4) {
        v.remove(d, format!("f{i}").as_bytes(), files[i]).unwrap();
    }
    v.rename(d, b"f1", files[1], ROOT, b"moved", None).unwrap();
    let held = files[2];
    v.hold(held).unwrap();
    v.remove(d, b"f2", held).unwrap();
    v.set_size(files[3], 100).unwrap();
    let s = v.create(d, b"sub", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    v.create(s, b"link", 0o120777, 0, 0, Kind::Symlink("z".repeat(80).as_bytes())).ok();
    v.rename(d, b"sub", s, ROOT, b"sub2", None).unwrap();
    for k in 0..60u64 {
        put(v, files[5], k * 8192, b"fragment");
    }
}

fn read_all(v: &Volume, dir: u32, depth: u32) {
    if depth > 8 {
        return;
    }
    let Ok(l) = v.list(dir) else { return };
    for (_, ino, ft) in l {
        if ft == crate::dir::FT_DIR {
            read_all(v, ino, depth + 1);
        } else if let Ok(st) = v.stat(ino) {
            let mut b = std::vec![0u8; st.size().min(1 << 20) as usize];
            let _ = v.read(ino, 0, &mut b);
        }
    }
}

fn check_disk(s: &Scratch, img: Vec<u8>, what: &str) {
    // V2's own mount of the cut disk: no panic, orphan cleanup, reading everything.
    let dev = Arc::new(MemDev::new(img.clone()));
    if let Ok(v) = Volume::mount(dev.clone(), opts()) {
        read_all(&v, ROOT, 0);
        let _ = v.sync_fs();
    }
    for (name, im) in [("raw cut", img), ("after V2 mounted it", dev.image())] {
        let (r1, r2, out) = repair(s, &im);
        assert!(r1 <= 1 && r2 == 0, "{what} ({name}): e2fsck -fy {r1}, then -fn {r2}\n{out}");
    }
}

fn layouts() -> [(&'static str, &'static [&'static str]); 2] {
    [("4k-csum", &["-b", "4096", "-O", "^has_journal"]), ("1k-32bit", &["-b", "1024", "-O", "^64bit,^metadata_csum"])]
}

#[test]
fn every_cut_is_repairable() {
    for (name, opts_) in layouts() {
        let s = Scratch::new();
        let base = mkfs(&s, "16M", opts_, None);
        let (dev, v) = mount(base);
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        changes(&v);
        let log = dev.take_log();
        let writes: Vec<_> = log.iter().filter(|(a, _)| *a != u64::MAX).collect();
        let step = (writes.len() / 25).max(1);
        let mut cuts = 0;
        for k in (0..writes.len()).step_by(step).chain([writes.len()]) {
            let mut img = before.clone();
            for (at, d) in &writes[..k] {
                img[*at as usize..*at as usize + d.len()].copy_from_slice(d);
            }
            check_disk(&s, img, &format!("{name}: cut after {k} of {} writes", writes.len()));
            cuts += 1;
        }
        assert!(cuts > 20);
    }
}

#[test]
fn a_device_that_stops_turns_the_volume_read_only() {
    let s = Scratch::new();
    let base = mkfs(&s, "16M", &["-O", "^has_journal"], None);
    for k in [3u64, 17, 60, 150] {
        let dev = Arc::new(MemDev::new(base.clone()));
        let v = Volume::mount(dev.clone(), opts()).unwrap();
        dev.stop_after(k);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| changes(&v)));
        assert!(r.is_err(), "the sequence must fail once the device stops");
        assert!(!v.writable(), "a failed write turns the volume read-only");
        assert_eq!(v.create(ROOT, b"after", 0o100644, 0, 0, Kind::Reg).err(), Some(crate::errno::EROFS));
        check_disk(&s, dev.image(), &format!("device stopped after {k} writes"));
        let _ = String::new();
    }
}

/// The harder sequence: one directory grows to 2000 names, a file is cut into more extents than two tree
/// levels hold (depth 2 at 4 KiB blocks), an htree directory (from `e2fsck -D`) is turned linear by an insert.
fn deep_changes(v: &Volume) -> u16 {
    let d = v.create(ROOT, b"grow", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    for i in 0..2000 {
        file(v, d, &format!("a-name-long-enough-to-fill-blocks-{i:05}"));
    }
    let f = file(v, ROOT, "fragmented");
    for k in 0..1500u64 {
        put(v, f, k * 8192, b"x");
    }
    let depth = crate::le::g16(v.stat(f).unwrap().i_block(), 6);
    let many = lookup(v, "/many");
    file(v, many, "into-the-htree");
    v.remove(many, b"entry-with-a-long-name-0007", lookup(v, "/many/entry-with-a-long-name-0007")).unwrap();
    for k in (0..1500u64).step_by(3) {
        v.set_size(f, (1500 - k) * 8192).unwrap();
    }
    depth
}

struct Rng(u64);
impl Rng {
    fn bit(&mut self) -> bool {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 & 1 == 1
    }
}

/// Each cut: every write before the last flush is on the disk, and of the writes after it (still in a
/// volatile cache) a random half, in any order: reordering a journal-less ext4 on Linux sees too.
#[test]
fn deep_cuts_with_reordered_writes_are_repairable() {
    for (name, opts_) in layouts() {
        let s = Scratch::new();
        let t = tree(&s);
        let img = mkfs(&s, "64M", opts_, Some(&t));
        let p = s.path("idx.img");
        std::fs::write(&p, &img).unwrap();
        assert!(std::process::Command::new("e2fsck").args(["-fyD"]).arg(&p).output().unwrap().status.code().unwrap() <= 1);
        let (dev, v) = mount(std::fs::read(&p).unwrap());
        assert!(v.stat(lookup(&v, "/many")).unwrap().flags() & crate::inode::FL_INDEX != 0, "{name}: /many not indexed");
        v.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        let depth_seen = deep_changes(&v);
        v.sync_fs().unwrap();
        let log = dev.take_log();
        drop(v);
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let step = (log.len() / 24).max(1);
        for k in (0..log.len()).step_by(step).chain([log.len()]) {
            let flush = log[..k].iter().rposition(|(a, _)| *a == u64::MAX).map_or(0, |i| i + 1);
            let mut img = before.clone();
            for (i, (at, d)) in log[..k].iter().enumerate() {
                if *at == u64::MAX || (i >= flush && !rng.bit()) {
                    continue;
                }
                img[*at as usize..*at as usize + d.len()].copy_from_slice(d);
            }
            check_disk(&s, img, &format!("{name}: cut after {k} of {} (flush at {flush}), half of the rest", log.len()));
        }
        if name == "4k-csum" {
            assert!(depth_seen >= 2, "the fragmented file reached depth {depth_seen}, not 2");
        }
    }
}
