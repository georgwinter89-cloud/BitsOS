//! Writing: names, data, links, truncate, the orphan list, htree directories turned linear; e2fsck -fn clean
//! after each sequence, and a fresh mount reads back what was written.

use super::basic::LAYOUTS;
use super::util::*;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::process::Command;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

pub fn put(v: &Volume, ino: u32, off: u64, data: &[u8]) {
    let size = v.stat(ino).unwrap().size().max(off + data.len() as u64);
    // Like the page cache: whole pages from a page boundary.
    let start = off & !4095;
    let mut buf = vec![0u8; (off + data.len() as u64 - start) as usize];
    let have = v.read(ino, start, &mut buf).unwrap_or(0);
    let _ = have;
    buf[(off - start) as usize..].copy_from_slice(data);
    let pages = start / 4096..(off + data.len() as u64).div_ceil(4096);
    v.write(ino, start, &buf, size, pages).unwrap();
}

pub fn file(v: &Volume, dir: u32, name: &str) -> u32 {
    v.create(dir, name.as_bytes(), 0o100644, 1000, 1000, Kind::Reg).unwrap().ino
}

fn pattern(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2654435761).wrapping_add(seed) >> 13) as u8).collect()
}

pub(super) fn sequence(v: &Volume) {
    let d = v.create(ROOT, b"work", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let big = v.create(d, b"big", 0o40700, 1000, 1000, Kind::Dir).unwrap().ino;
    for i in 0..1500 {
        let f = file(v, big, &format!("file-with-a-rather-long-name-{i:05}"));
        if i % 50 == 0 {
            put(v, f, 0, format!("content {i}").as_bytes());
        }
    }
    for i in (0..1500).step_by(3) {
        let name = format!("file-with-a-rather-long-name-{i:05}");
        let ino = v.list(big).unwrap().into_iter().find(|(n, _, _)| n == name.as_bytes()).unwrap().1;
        v.remove(big, name.as_bytes(), ino).unwrap();
    }
    let data = file(v, d, "data.bin");
    let p = pattern(3_000_000, 1);
    put(v, data, 0, &p);
    // A fragmented file: every other block, so the extent tree needs index levels.
    let frag = file(v, d, "frag");
    let bs = v.geo.bs;
    for k in 0..900u64 {
        put(v, frag, k * 2 * bs.max(4096), &pattern(100, k as u32));
    }
    put(v, frag, 7 * 4096 + 5, b"into a hole");
    v.create(d, b"fast", 0o120777, 0, 0, Kind::Symlink(b"data.bin")).unwrap();
    v.create(d, b"slow", 0o120777, 0, 0, Kind::Symlink("y/".repeat(100).as_bytes())).unwrap();
    v.create(d, b"fifo", 0o10644, 0, 0, Kind::Fifo).unwrap();
    v.create(d, b"null", 0o20666, 0, 0, Kind::Dev((1 << 8) | 3)).unwrap();
    v.create(d, b"bigdev", 0o60600, 0, 0, Kind::Dev((300 << 8) | 0x1234_5)).unwrap();
    v.link(big, b"hard", data).unwrap();
    v.rename(d, b"data.bin", data, big, b"moved.bin", None).unwrap();
    let sub = v.create(d, b"sub", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    v.rename(d, b"sub", sub, big, b"sub-moved", None).unwrap();
    let over = file(v, d, "over");
    put(v, over, 0, b"old");
    let new = file(v, d, "new");
    put(v, new, 0, b"new content");
    v.rename(d, b"new", new, d, b"over", Some(over)).unwrap();
    let a = v.create(d, b"a", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let b = file(v, big, "b");
    v.exchange(d, b"a", a, big, b"b", b).unwrap();
    v.set_size(data, 1_000_001).unwrap();
    v.set_size(data, 1_500_000).unwrap();
    v.set_size(frag, 3 * bs).unwrap();
    v.remove(big, b"hard", data).unwrap();
    let e = v.create(d, b"empty-dir", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    v.remove(d, b"empty-dir", e).unwrap();
    let names: Vec<String> = v.list(d).unwrap().into_iter().map(|(n, i, _)| format!("{}={i}", String::from_utf8_lossy(&n))).collect();
    assert_eq!(v.remove(d, b"big", big).err(), Some(crate::errno::ENOTEMPTY), "big={big} in {names:?}");
}

pub(super) fn readback(v: &Volume) {
    let data = lookup(v, "work/big/moved.bin");
    let st = v.stat(data).unwrap();
    assert_eq!(st.size(), 1_500_000);
    assert_eq!(st.links(), 1);
    let mut b = vec![0u8; 1_500_000];
    v.read(data, 0, &mut b).unwrap();
    assert_eq!(&b[..1_000_001], &pattern(3_000_000, 1)[..1_000_001]);
    assert!(b[1_000_001..].iter().all(|&x| x == 0), "tail after truncate up must read zero");
    let over = lookup(v, "work/over");
    let mut o = vec![0u8; 20];
    let n = v.read(over, 0, &mut o).unwrap();
    assert_eq!(&o[..n], b"new content");
    assert_eq!(v.readlink(lookup(v, "work/slow")).unwrap(), "y/".repeat(100).as_bytes());
    assert_eq!(crate::vol_ns::rdev_of(&v.stat(lookup(v, "work/bigdev")).unwrap()), (300 << 8) | 0x1234_5);
    assert_eq!(crate::vol_ns::rdev_of(&v.stat(lookup(v, "work/null")).unwrap()), (1 << 8) | 3);
    assert!(v.stat(lookup(v, "work/a")).unwrap().is_reg());
    assert!(v.stat(lookup(v, "work/big/b")).unwrap().is_dir());
    assert_eq!(v.list(lookup(v, "work/big")).unwrap().len(), 1000 + 3);
}

#[test]
fn write_sequence_is_fsck_clean_on_every_layout() {
    for (name, opts) in LAYOUTS {
        let s = Scratch::new();
        let img = mkfs(&s, "128M", opts, None);
        let (dev, v) = mount(img);
        sequence(&v);
        readback(&v);
        v.sync_fs().unwrap();
        let img = dev.image();
        let (c, out) = fsck(&s, &img);
        assert_eq!(c, 0, "{name}: e2fsck after the write sequence:\n{out}");
        let (_d2, v2) = mount(img);
        readback(&v2);
    }
}

#[test]
fn open_unlinked_file_lives_on_the_orphan_list() {
    let s = Scratch::new();
    let img = mkfs(&s, "32M", &[], None);
    let (dev, v) = mount(img);
    let f = file(&v, ROOT, "tmp");
    v.hold(f).unwrap();
    put(&v, f, 0, &pattern(200_000, 3));
    let free_before = v.statfs().unwrap().bfree;
    v.remove(ROOT, b"tmp", f).unwrap();
    // Still readable while held; on the orphan list on disk.
    let mut b = vec![0u8; 10];
    assert_eq!(v.read(f, 0, &mut b).unwrap(), 10);
    assert_eq!(crate::le::g32(&v.st.lock().sb, crate::sb::LAST_ORPHAN), f);
    // A cut now: the next mount frees it (orphan cleanup), and e2fsck finds nothing.
    let cut = dev.image();
    let (d2, v2) = mount(cut);
    assert_eq!(crate::le::g32(&v2.st.lock().sb, crate::sb::LAST_ORPHAN), 0);
    v2.sync_fs().unwrap();
    assert_clean(&s, &d2.image(), "orphan cleanup at mount");
    v.forget(f);
    let _ = v.stat(ROOT);
    assert!(v.statfs().unwrap().bfree > free_before);
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "forget of an unlinked file");
}

#[test]
fn inserting_into_an_htree_directory_turns_it_linear() {
    let s = Scratch::new();
    let t = tree(&s);
    let img = mkfs(&s, "64M", &[], Some(&t));
    let p = s.path("idx.img");
    std::fs::write(&p, &img).unwrap();
    Command::new("e2fsck").args(["-fyD"]).arg(&p).output().unwrap();
    let img = std::fs::read(&p).unwrap();
    let (dev, v) = mount(img);
    let many = lookup(&v, "many");
    assert_ne!(v.stat(many).unwrap().flags() & crate::inode::FL_INDEX, 0);
    let n = file(&v, many, "new-name");
    let sub = v.create(many, b"subdir", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    v.rename(many, b"subdir", sub, ROOT, b"subdir-out", None).unwrap();
    let victim = lookup(&v, "many/entry-with-a-long-name-0007");
    v.remove(many, b"entry-with-a-long-name-0007", victim).unwrap();
    assert_eq!(v.stat(many).unwrap().flags() & crate::inode::FL_INDEX, 0);
    let names: Vec<String> = v.list(many).unwrap().into_iter().map(|(n, _, _)| String::from_utf8_lossy(&n).into_owned()).filter(|n| !n.starts_with("entry-with-a-long-name-0")).collect();
    assert_eq!(v.list(many).unwrap().len(), 600, "{names:?}");
    assert!(n > 0);
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "insert into an htree directory");
    let _ = String::new();
}

#[test]
fn full_volume_answers_enospc_and_stays_clean() {
    let s = Scratch::new();
    let img = mkfs(&s, "8M", &["-b", "1024", "-m", "0"], None);
    let (dev, v) = mount(img);
    let f = file(&v, ROOT, "fill");
    v.hold(f).unwrap();
    let chunk = vec![0x5Au8; 256 * 1024];
    let mut off = 0u64;
    let mut hit = false;
    for _ in 0..64 {
        let pages_first = off / 4096;
        let last = (off + chunk.len() as u64 - 1) / 4096;
        match v.reserve(f, pages_first, last) {
            Ok(()) => {
                put(&v, f, off, &chunk);
                off += chunk.len() as u64;
            }
            Err(e) => {
                assert_eq!(e, crate::errno::ENOSPC);
                hit = true;
                break;
            }
        }
    }
    assert!(hit, "8 MiB never filled");
    // Names still work in the root reserve... and many small files until inodes or blocks run out.
    let mut made = 0;
    for i in 0..5000 {
        match v.create(ROOT, format!("n{i}").as_bytes(), 0o100644, 0, 0, Kind::Reg) {
            Ok(_) => made += 1,
            Err(e) => {
                assert_eq!(e, crate::errno::ENOSPC);
                break;
            }
        }
    }
    assert!(made > 0);
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "a full volume");
}

