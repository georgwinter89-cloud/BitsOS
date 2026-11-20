//! Generator of crates/fs-ext4/testdata/old-*.img.z (FS-W): images written by the code BEFORE FS-W (branch
//! fix/nofile, 58c26e16, generation 121). Not part of the tree: dropped into that revision's
//! crates/fs-ext4/src/tests/ (plus `mod oldimg;` in tests/mod.rs) and run once with
//! `OLD_IMAGES_OUT=<dir> cargo test -p fs-ext4 -- tests::oldimg --nocapture`.
//! `tests/old_image.rs` of the current tree mounts what it wrote.

use super::util::*;
use super::write::{file, put};
use crate::inode::ROOT;
use crate::Kind;
use std::vec::Vec;

/// A file's bytes: constant per 4 KiB block, so the images pack small.
fn fill(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| seed ^ (i / 4096) as u8).collect()
}

fn save(name: &str, img: &[u8]) {
    let dir = std::env::var("OLD_IMAGES_OUT").expect("OLD_IMAGES_OUT");
    let z = miniz_oxide::deflate::compress_to_vec_zlib(img, 9);
    std::fs::write(std::path::Path::new(&dir).join(name), &z).unwrap();
    std::eprintln!("{name}: {} bytes packed", z.len());
}

fn lay(before: &[u8], log: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut img = before.to_vec();
    for (at, d) in log {
        if *at != u64::MAX {
            img[*at as usize..*at as usize + d.len()].copy_from_slice(d);
        }
    }
    img
}

/// With a journal: two commits at home, an open-unlinked file on the orphan list, and a third commit that is in
/// the log only (the cut falls right behind its second flush, before the home writes).
#[test]
fn old_journal_image() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "16M", &["-b", "4096"], None));
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let a = file(&v, d, "a");
    put(&v, a, 0, &fill(300 << 10, 1));
    let b = file(&v, d, "b");
    put(&v, b, 0, &fill(70_000, 2));
    v.flush().unwrap();
    let c = file(&v, d, "c");
    let cd = fill(5 << 18, 3);
    for at in (0..1 << 20).step_by(1 << 18) {
        put(&v, c, at as u64, &cd[at..at + (1 << 18)]);
    }
    v.set_size(b, 5000).unwrap();
    v.rename(d, b"a", a, ROOT, b"moved", None).unwrap();
    let e = file(&v, d, "e");
    put(&v, e, 0, &fill(100 << 10, 5));
    v.hold(e).unwrap();
    v.remove(d, b"e", e).unwrap();
    v.flush().unwrap();
    let before = dev.image();
    dev.record();
    let g = file(&v, d, "g");
    put(&v, g, 0, &fill(200 << 10, 7));
    put(&v, c, 1 << 20, &cd[1 << 20..]);
    v.flush().unwrap();
    let log = dev.take_log();
    let last = log.iter().rposition(|(at, _)| *at == u64::MAX).unwrap();
    let img = lay(&before, &log[..last]);
    let (r1, r2, out) = repair(&s, &img);
    assert!(r1 <= 1 && r2 == 0, "e2fsck's replay: -fy {r1}, -fn {r2}\n{out}");
    save("old-journal.img.z", &img);
}

/// Without a journal: the old order of a data write was allocation on the disk, data, pointers. The cut falls
/// between the first two: a megabyte of blocks is marked used and nothing points at it.
#[test]
fn old_bare_image() {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "16M", &["-b", "4096", "-O", "^has_journal"], None));
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let a = file(&v, d, "a");
    put(&v, a, 0, &fill(300 << 10, 1));
    let b = file(&v, d, "b");
    put(&v, b, 0, &fill(70_000, 2));
    let big = file(&v, d, "big");
    v.flush().unwrap();
    let before = dev.image();
    dev.record();
    put(&v, big, 0, &fill(1 << 20, 9));
    let log = dev.take_log();
    let data = log.iter().position(|(_, d)| d.len() >= 64 << 10).expect("the data write");
    assert!(data > 0, "no allocation was written before the data");
    let img = lay(&before, &log[..data]);
    let (code, out) = fsck(&s, &img);
    assert!(code != 0 && out.contains("differences"), "the cut image has no leaked blocks: e2fsck -fn {code}\n{out}");
    let (r1, r2, out) = repair(&s, &img);
    assert!(r1 <= 1 && r2 == 0, "e2fsck -fy {r1}, -fn {r2}\n{out}");
    save("old-bare.img.z", &img);
}
