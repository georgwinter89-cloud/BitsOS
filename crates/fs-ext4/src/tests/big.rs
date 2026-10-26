//! Large files: a 5 GiB sparse file with data beyond 4 GiB, and a 200 MiB file written in 1 MiB runs (long
//! extents, capped at 32768 blocks each as Linux caps them).

use super::util::*;
use super::write::{file, put};
use crate::inode::ROOT;
use std::vec;

#[test]
fn five_gib_sparse_file() {
    let s = Scratch::new();
    let img = mkfs(&s, "64M", &["-b", "4096"], None);
    let (dev, v) = mount(img);
    let f = file(&v, ROOT, "five-gib");
    let five = 5u64 << 30;
    for (at, what) in [(0u64, &b"start"[..]), (4u64 << 30, b"at four gib"), ((4u64 << 30) + 4096 * 3 + 17, b"unaligned past 4 GiB"), (five - 7, b"the end")] {
        put(&v, f, at, what);
    }
    let st = v.stat(f).unwrap();
    assert_eq!(st.size(), five);
    assert!(st.blocks512(&v.geo) <= 6 * 8, "sparse: {} sectors", st.blocks512(&v.geo));
    let mut b = vec![0u8; 20];
    v.read(f, (4u64 << 30) + 4096 * 3 + 17, &mut b).unwrap();
    assert_eq!(&b, b"unaligned past 4 GiB");
    let mut e = [0u8; 7];
    assert_eq!(v.read(f, five - 7, &mut e).unwrap(), 7);
    assert_eq!(&e, b"the end");
    let mut z = vec![1u8; 4096];
    v.read(f, 3u64 << 30, &mut z).unwrap();
    assert!(z.iter().all(|&x| x == 0));
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "a 5 GiB sparse file");
    let out = debugfs(&s, &dev.image(), "stat /five-gib");
    assert!(out.contains("Size: 5368709120"), "{out}");
    // Truncate below 4 GiB, then remount: still clean.
    v.set_size(f, (4u64 << 30) - 1).unwrap();
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "truncating it below 4 GiB");
}

#[test]
fn two_hundred_mib_in_long_extents() {
    let s = Scratch::new();
    let img = mkfs(&s, "300M", &["-b", "4096", "-O", "^has_journal"], None);
    let (dev, v) = mount(img);
    let f = file(&v, ROOT, "big");
    let run = vec![0xC3u8; 1 << 20];
    for k in 0..200u64 {
        put(&v, f, k << 20, &run);
    }
    let st = v.stat(f).unwrap();
    assert_eq!(st.size(), 200 << 20);
    let exts = v.st.lock().extents(&st).unwrap();
    assert!(exts.len() <= 8, "{} extents for 200 MiB", exts.len());
    assert!(exts.iter().all(|e| e.len <= 32768));
    let mut b = vec![0u8; 1 << 20];
    v.read(f, 150 << 20, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0xC3));
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "200 MiB written");
    v.remove(ROOT, b"big", f).unwrap();
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "removing it");
}
