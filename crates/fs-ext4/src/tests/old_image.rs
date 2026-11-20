//! FS-W changed no byte of the on-disk format, only the order in which a write-back step's blocks reach the disk.
//! Proof on images the code before FS-W wrote (fix/nofile 58c26e16, generation 121; made by
//! `testdata/old_images_gen.rs`, files constant per block so they pack small): they mount, replay and read
//! with the new code, take new writes, and check clean.

use super::flight::{read_all, step};
use super::util::*;
use super::write::file;
use crate::inode::ROOT;
use std::vec::Vec;

static JOURNAL: &[u8] = include_bytes!("../../testdata/old-journal.img.z");
static BARE: &[u8] = include_bytes!("../../testdata/old-bare.img.z");

/// The generator's file contents.
fn fill(n: usize, seed: u8) -> Vec<u8> {
    (0..n).map(|i| seed ^ (i / 4096) as u8).collect()
}

fn unpack(z: &[u8]) -> Vec<u8> {
    miniz_oxide::inflate::decompress_to_vec_zlib(z).expect("the packed image")
}

/// The old code's last commit is in the log only, and a file it unlinked while open is on the orphan list: the
/// new code replays the one, frees the other, and writes on.
#[test]
fn an_image_with_the_old_codes_log_replays_and_takes_new_writes() {
    let s = Scratch::new();
    let img = unpack(JOURNAL);
    let replays = crate::journal::replays();
    let (dev, v) = mount(img);
    assert!(crate::journal::replays() > replays, "the old log was not replayed");
    let c = lookup(&v, "/d/c");
    assert!(read_all(&v, lookup(&v, "/moved")) == fill(300 << 10, 1), "/moved");
    assert!(read_all(&v, lookup(&v, "/d/b")) == fill(70_000, 2)[..5000], "/d/b");
    assert!(read_all(&v, c) == fill(5 << 18, 3), "/d/c with the step of the old log");
    assert!(read_all(&v, lookup(&v, "/d/g")) == fill(200 << 10, 7), "/d/g of the old log");
    assert!(v.list(lookup(&v, "/d")).unwrap().iter().all(|(n, _, _)| n != b"e"), "the unlinked file has a name");

    // New steps: behind the old file's end, over its middle, and a new file.
    let more = fill(1 << 20, 0x40);
    step(&v, c, 5 << 18, &more, (5 << 18) + (1 << 20)).unwrap();
    step(&v, c, 1 << 18, &more[..8192], (5 << 18) + (1 << 20)).unwrap();
    let n = file(&v, ROOT, "new");
    step(&v, n, 0, &more[..100_000], 100_000).unwrap();
    v.sync_fs().unwrap();
    drop(v);
    let img = dev.image();
    assert_clean(&s, &img, "the old journal image after the new code's writes");
    let (_d2, v2) = mount(img);
    let mut want = fill(5 << 18, 3);
    want.extend_from_slice(&more);
    want[1 << 18..(1 << 18) + 8192].copy_from_slice(&more[..8192]);
    assert!(read_all(&v2, lookup(&v2, "/d/c")) == want, "/d/c after the new steps");
    assert!(read_all(&v2, lookup(&v2, "/new")) == more[..100_000], "/new");
}

/// Without a journal the old order was allocation on the disk, then data, then pointers. The image is cut
/// between the first two: a megabyte of blocks marked used that nothing points at. The new code mounts it, never
/// hands those blocks out, writes the file again, and e2fsck's repair (the bitmap differences the old cut left)
/// loses nothing.
#[test]
fn an_image_with_the_old_codes_half_done_allocation_takes_new_writes() {
    let s = Scratch::new();
    let img = unpack(BARE);
    let (code, out) = fsck(&s, &img);
    assert!(code != 0 && out.contains("differences"), "the image has no blocks left over by the old order: {code}\n{out}");
    let (dev, v) = mount(img);
    let big = lookup(&v, "/d/big");
    assert_eq!(v.stat(big).unwrap().size(), 0, "/d/big: the old cut was before its pointers");
    assert!(read_all(&v, lookup(&v, "/d/a")) == fill(300 << 10, 1), "/d/a");
    assert!(read_all(&v, lookup(&v, "/d/b")) == fill(70_000, 2), "/d/b");
    let free = v.st.lock().free_now();

    let data = fill(1 << 20, 0x40);
    step(&v, big, 0, &data, 1 << 20).unwrap();
    let n = file(&v, ROOT, "new");
    step(&v, n, 0, &data[..100_000], 100_000).unwrap();
    assert_eq!(v.st.lock().free_now(), free - 256 - 25, "the new steps' blocks");
    v.sync_fs().unwrap();
    drop(v);
    let (r1, r2, out) = repair(&s, &dev.image());
    assert!(r1 <= 1 && r2 == 0, "e2fsck -fy {r1}, then -fn {r2}\n{out}");
    let (_d2, v2) = mount(std::fs::read(s.path("repair.img")).unwrap());
    assert!(read_all(&v2, lookup(&v2, "/d/big")) == data, "/d/big after the repair");
    assert!(read_all(&v2, lookup(&v2, "/new")) == data[..100_000], "/new after the repair");
    // The repair gave the old code's megabyte back.
    assert_eq!(v2.st.lock().free_now(), free - 256 - 25 + 256, "free blocks after the repair");
}
