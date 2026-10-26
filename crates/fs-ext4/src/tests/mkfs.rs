//! FS-8c: V2's own mkfs. Each made volume is e2fsck-clean, has the feature set of `mkfs`'s doc, mounts, takes
//! the write sequence of `write.rs` and is clean again after it; a cut during mkfs leaves nothing mountable.

use super::util::*;
use super::write::{readback, sequence};
use crate::dev::{Dev, MemDev};
use crate::mkfs::{format, MkfsOptions};
use crate::Volume;
use std::process::Command;
use std::string::String;
use std::sync::Arc;
use std::vec;

pub fn opts_for(zeroed: bool) -> MkfsOptions {
    let mut o = MkfsOptions::new(b"bitsdata", [0x5a; 16], [0x33; 16], now());
    o.uuid[6] = 0x40;
    o.zeroed = zeroed;
    o
}

fn dumpe2fs(s: &Scratch, img: &[u8]) -> String {
    let p = s.path("dump.img");
    std::fs::write(&p, img).unwrap();
    let o = Command::new("dumpe2fs").env("LC_ALL", "C").arg("-h").arg(&p).output().unwrap();
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn mkfs_is_clean_on_many_sizes() {
    // One short group, exactly one group, several with a short last one, a last one too short (dropped),
    // and enough groups for a second descriptor block's worth of backups (sparse_super 1, 3, 5, 7, 9, 25, 27).
    for mib in [8u64, 16, 128, 300, 513, 1100] {
        let s = Scratch::new();
        let dev = MemDev::new(vec![0u8; (mib << 20) as usize]);
        let made = format(&dev, &opts_for(mib % 2 == 0)).unwrap();
        let img = dev.image();
        let (c, out) = fsck(&s, &img);
        assert_eq!(c, 0, "{mib} MiB: e2fsck after mkfs:\n{out}");
        let d = dumpe2fs(&s, &img);
        for f in ["has_journal", "ext_attr", "dir_index", "filetype", "extent", "64bit", "sparse_super", "large_file", "huge_file", "dir_nlink", "extra_isize", "metadata_csum"] {
            assert!(d.lines().any(|l| l.starts_with("Filesystem features:") && l.split_whitespace().any(|w| w == f)), "{mib} MiB: feature {f} missing:\n{d}");
        }
        assert!(d.contains("Filesystem volume name:   bitsdata"), "{d}");
        assert!(d.contains("Filesystem state:         clean"), "{d}");
        let v = Volume::mount(Arc::new(MemDev::new(img)), opts()).unwrap();
        let st = v.statfs().unwrap();
        assert_eq!(st.bfree, made.free_blocks, "{mib} MiB");
        assert!(made.journal_blocks >= 1024);
    }
}

#[test]
fn mkfs_on_dirty_device_zeroes_what_it_needs() {
    // A device full of old bytes: not `zeroed`, so mkfs writes the inode tables and the journal.
    let s = Scratch::new();
    let dev = MemDev::new(vec![0xA5u8; 64 << 20]);
    format(&dev, &opts_for(false)).unwrap();
    assert_clean(&s, &dev.image(), "mkfs on a dirty device");
}

#[test]
fn mkfs_volume_takes_the_write_sequence() {
    let s = Scratch::new();
    let dev = Arc::new(MemDev::new(vec![0u8; 128 << 20]));
    format(&*dev, &opts_for(true)).unwrap();
    let v = Volume::mount(dev.clone(), opts()).unwrap();
    sequence(&v);
    readback(&v);
    v.sync_fs().unwrap();
    let img = dev.image();
    assert_clean(&s, &img, "the write sequence on a V2-made volume");
    let (_d, v2) = mount(img);
    readback(&v2);
}

#[test]
fn mkfs_volume_journal_replays_clean() {
    // Commit without checkpoint (the state after sync(2)), then e2fsck replays and finds nothing else.
    let s = Scratch::new();
    let dev = Arc::new(MemDev::new(vec![0u8; 64 << 20]));
    format(&*dev, &opts_for(true)).unwrap();
    let v = Volume::mount(dev.clone(), opts()).unwrap();
    let d = v.create(crate::inode::ROOT, b"d", 0o40755, 1000, 1000, crate::Kind::Dir).unwrap().ino;
    for i in 0..50 {
        super::write::file(&v, d, &std::format!("f{i}"));
    }
    v.flush().unwrap();
    let img = dev.image();
    let (_rep, after, out) = repair(&s, &img);
    assert_eq!(after, 0, "after replaying the journal: {out}");
    let (_d, v2) = mount(img);
    assert_eq!(v2.list(d).unwrap().len(), 50, "replayed by V2 at mount");
}

#[test]
fn mkfs_cut_leaves_nothing_mountable_or_a_clean_volume() {
    let dev = MemDev::new(vec![0u8; 32 << 20]);
    dev.record();
    format(&dev, &opts_for(true)).unwrap();
    let log = dev.take_log();
    let writes: std::vec::Vec<_> = log.iter().filter(|(at, _)| *at != u64::MAX).collect();
    for k in 0..writes.len() {
        let img = MemDev::new(vec![0u8; 32 << 20]);
        for (at, data) in &writes[..k] {
            img.write(*at, data).unwrap();
        }
        assert!(Volume::mount(Arc::new(img), opts()).is_err(), "a mkfs cut after {k} of {} writes mounted", writes.len());
    }
}

#[test]
fn mkfs_refuses_tiny_devices() {
    assert_eq!(format(&MemDev::new(vec![0u8; 4 << 20]), &opts_for(true)).err(), Some(crate::errno::EINVAL));
}
