//! ext3 files on an ext4 volume (mkfs.ext3, then `tune2fs -O extents`): block-mapped files and directories
//! are turned into extent trees when V2 changes them; e2fsck -fn clean and the content read back after.

use super::util::*;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::process::Command;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};

fn ext3_with_extents(s: &Scratch, bs: &str) -> Vec<u8> {
    let d = s.path("e3tree");
    std::fs::create_dir_all(d.join("d")).unwrap();
    for i in 1..=40u32 {
        std::fs::write(d.join(format!("d/f{i}")), (0..i * 7000).map(|k| (k * 13 + i) as u8).collect::<Vec<u8>>()).unwrap();
    }
    // past the single and into the double indirect block at 1 KiB blocks
    std::fs::write(d.join("big"), (0..9_000_000u32).map(|k| (k * 7 % 251) as u8).collect::<Vec<u8>>()).unwrap();
    std::os::unix::fs::symlink("y/".repeat(70), d.join("slow")).unwrap();
    let img = s.path("e3.img");
    let _ = std::fs::remove_file(&img);
    assert!(Command::new("truncate").args(["-s", "48M"]).arg(&img).status().unwrap().success());
    let o = Command::new("mkfs.ext3").env("E2FSPROGS_FAKE_TIME", "1700000000").args(["-q", "-F", "-b", bs, "-d"]).arg(&d).arg(&img).output().unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    assert!(Command::new("tune2fs").args(["-O", "extents"]).arg(&img).output().unwrap().status.success());
    let b = std::fs::read(&img).unwrap();
    assert_clean(s, &b, "ext3 with extents feature");
    b
}

/// Like the page cache: whole pages (up to the size) around `data`.
fn put(v: &Volume, ino: u32, off: u64, data: &[u8]) {
    let size = v.stat(ino).unwrap().size().max(off + data.len() as u64);
    let start = off & !4095;
    let end = (off + data.len() as u64).div_ceil(4096).saturating_mul(4096).min(size);
    let mut buf = vec![0u8; (end - start) as usize];
    let _ = v.read(ino, start, &mut buf);
    buf[(off - start) as usize..(off - start) as usize + data.len()].copy_from_slice(data);
    v.write(ino, start, &buf, size, start / 4096..end.div_ceil(4096)).unwrap();
}

fn read_all(v: &Volume, ino: u32) -> Vec<u8> {
    let mut b = vec![0u8; v.stat(ino).unwrap().size() as usize];
    assert_eq!(v.read(ino, 0, &mut b).unwrap(), b.len());
    b
}

#[test]
fn block_mapped_files_become_extents() {
    for bs in ["1024", "4096"] {
        let s = Scratch::new();
        let (dev, v) = mount(ext3_with_extents(&s, bs));
        let big = lookup(&v, "/big");
        let d = lookup(&v, "/d");
        let mut want_big = read_all(&v, big);
        assert!(!crate::extent::uses_extents(&v.stat(big).unwrap()));
        // overwrite in the double-indirect range, append
        let at = 5_000_000u64;
        put(&v, big, at, &[0xEE; 10_000]);
        want_big[at as usize..at as usize + 10_000].fill(0xEE);
        let end = want_big.len() as u64;
        put(&v, big, end, b"appended");
        want_big.extend_from_slice(b"appended");
        assert!(crate::extent::uses_extents(&v.stat(big).unwrap()));
        // truncate, unlink, a new name in the old directory, rename into it, unlink the slow symlink
        let f5 = lookup(&v, "/d/f5");
        v.set_size(f5, 1000).unwrap();
        let f6 = lookup(&v, "/d/f6");
        v.remove(d, b"f6", f6).unwrap();
        for i in 0..200 {
            v.create(d, format!("new-name-number-{i:04}").as_bytes(), 0o100644, 0, 0, Kind::Reg).unwrap();
        }
        let f7 = lookup(&v, "/d/f7");
        v.rename(d, b"f7", f7, ROOT, b"f7-moved", None).unwrap();
        let slow = lookup(&v, "/slow");
        v.remove(ROOT, b"slow", slow).unwrap();
        v.sync_fs().unwrap();
        let img = dev.image();
        assert_clean(&s, &img, &format!("ext3 files changed, bs {bs}"));
        let (_d2, v2) = mount(img);
        let got = read_all(&v2, lookup(&v2, "/big"));
        assert_eq!(got.len(), want_big.len(), "bs {bs}: size");
        let diff = got.iter().zip(&want_big).position(|(a, b)| a != b);
        assert!(diff.is_none(), "bs {bs}: big differs first at byte {diff:?}");
        let f5b = read_all(&v2, lookup(&v2, "/d/f5"));
        assert_eq!(f5b, (0..1000u32).map(|k| (k * 13 + 5) as u8).collect::<Vec<u8>>());
        assert_eq!(read_all(&v2, lookup(&v2, "/f7-moved")).len(), 7 * 7000);
        assert_eq!(v2.list(lookup(&v2, "/d")).unwrap().len(), 39 - 1 + 200);
    }
}
