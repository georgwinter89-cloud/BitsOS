//! `write_run` in bounded steps (FS-W33): one kept buffer of `WRITE_STEP_PAGES`, not one as long as the run. The device's
//! bytes are counted, because a step that wrote a byte twice would look right to any test on the content (FAT's first
//! split wrote a growing file twice); a cut at any write still finds written data under the size on the disk.

use super::cut::check_disk;
use super::util::*;
use super::vfs::fresh_ns;
use crate::{MemDev, Volume};
use std::collections::BTreeSet;
use std::format;
use std::sync::Arc;
use std::vec::Vec;
use vfs::abi::{O_CREAT, O_RDONLY, O_TRUNC, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

const MIB: usize = 1024 * 1024;
const STEP: u64 = crate::file::WRITE_STEP_PAGES as u64 * 4096;
const BIG: &[u8] = b"/data/big";

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len).map(|i| ((i / 4096) as u32).to_le_bytes()[i % 4] ^ (i as u8) ^ seed).collect()
}

fn fsync(ns: &vfs::MountNs) {
    ns.lookup("/data/big").unwrap().inode.sync(SyncKind::Fsync).unwrap();
}

fn read_back(dev: &MemDev, len: usize) -> Vec<u8> {
    let (_d, v) = mount(dev.image());
    let ino = lookup(&v, "/big");
    assert_eq!(v.stat(ino).unwrap().size(), len as u64);
    let mut got = std::vec![0u8; len];
    assert_eq!(v.read(ino, 0, &mut got).unwrap(), len);
    got
}

/// Bytes on the device for `f`, which must make `want` dirty bytes: at least those, and only what metadata adds.
fn counted(what: &str, dev: &MemDev, want: usize, f: impl FnOnce()) {
    let before = dev.written_bytes();
    f();
    let written = dev.written_bytes() - before;
    let limit = want as u64 / 20 * 21;
    assert!(written >= want as u64, "{what}: only {written} bytes on the device for {want}");
    assert!(written <= limit, "{what}: {written} bytes on the device for {want}");
}

#[test]
fn the_steps_of_a_long_run_write_each_byte_once() {
    let layouts: [(&str, &[&str]); 3] = [("4k-journal", &["-b", "4096"]), ("4k-bare", &["-b", "4096", "-O", "^has_journal"]), ("1k-journal", &["-b", "1024", "-O", "^64bit,^metadata_csum"])];
    for (name, extra) in layouts {
        let s = Scratch::new();
        let (dev, ns, vol, _m) = fresh_ns(&s, "120M", extra);
        let c = Ctx::kernel(&ns);
        let mut want = pattern(49 * MIB + 123, 11);

        // A new file, many steps.
        let f = c.open(None, BIG, O_WRONLY | O_CREAT, 0o644).unwrap();
        assert_eq!(f.write(&want).unwrap(), want.len());
        drop(f);
        counted(&format!("{name}: new"), &dev, want.len(), || fsync(&ns));
        assert_eq!(vol.st.lock().reserved, 0, "{name}: promises left over");
        if name.starts_with("4k") {
            let di = vol.stat(lookup(&vol, "/big")).unwrap();
            let exts = vol.st.lock().extents(&di).unwrap();
            // The free space decides where the file breaks (the first gaps of a fresh disk), never a step's end.
            assert!(exts.iter().all(|e| e.lblk == 0 || e.lblk as u64 % (STEP / 4096) != 0), "{name}: a step's end cut the file: {exts:?}");
        }
        assert!(read_back(&dev, want.len()) == want, "{name}: the file differs after the remount");

        // Its first 20 MiB again: steps that end far below the file's end.
        let part = pattern(20 * MIB, 12);
        want[..part.len()].copy_from_slice(&part);
        let f = c.open(None, BIG, O_WRONLY, 0).unwrap();
        assert_eq!(f.write(&part).unwrap(), part.len());
        drop(f);
        counted(&format!("{name}: rewrite"), &dev, part.len(), || fsync(&ns));
        assert!(read_back(&dev, want.len()) == want, "{name}: the rewritten file differs");

        // A short file growing long, with a tail page that stays short.
        let short = pattern(1000, 13);
        let f = c.open(None, BIG, O_WRONLY | O_TRUNC, 0).unwrap();
        assert_eq!(f.write(&short).unwrap(), short.len());
        fsync(&ns);
        let more = pattern(35 * MIB + 77, 14);
        assert_eq!(f.write(&more).unwrap(), more.len());
        drop(f);
        counted(&format!("{name}: grow"), &dev, more.len(), || fsync(&ns));
        let mut all = short;
        all.extend_from_slice(&more);
        assert!(read_back(&dev, all.len()) == all, "{name}: the grown file differs");
        vol.sync_fs().unwrap();
        assert_clean(&s, &dev.image(), name);
    }
}

/// What a disk cut after some writes holds: every byte under the file's size is the run's own (never another file's
/// old block, never a zero the run did not write). Returns that size.
fn data_under_the_size(img: Vec<u8>, want: &[u8], what: &str) -> u64 {
    let dev = Arc::new(MemDev::new(img));
    let v = Volume::mount(dev, opts()).unwrap_or_else(|e| panic!("{what}: mount {e}"));
    let ino = lookup(&v, "/big");
    let size = v.stat(ino).unwrap().size();
    assert!(size <= want.len() as u64, "{what}: size {size}");
    let mut got = std::vec![0u8; size as usize];
    assert_eq!(v.read(ino, 0, &mut got).unwrap(), got.len(), "{what}");
    assert!(got == want[..got.len()], "{what}: the data under the size {size} is not the run's");
    size
}

/// The device stops between two steps: the call says so, and what the disk has is the first steps' data under their size.
#[test]
fn a_device_that_stops_between_steps() {
    let s = Scratch::new();
    let (dev, ns, _vol, _m) = fresh_ns(&s, "64M", &["-b", "4096", "-O", "^has_journal"]);
    let c = Ctx::kernel(&ns);
    let want = pattern(36 * MIB, 31);
    let f = c.open(None, BIG, O_WRONLY | O_CREAT, 0o644).unwrap();
    assert_eq!(f.write(&want).unwrap(), want.len());
    drop(f);
    fsync_refused_after(&dev, &ns, 24);
    let size = data_under_the_size(dev.image(), &want, "stopped after 24 writes");
    assert!(size > 0 && size < want.len() as u64 && size % STEP == 0, "the size {size} is not the end of one of the first steps");
    check_disk(&s, dev.image(), "stopped after 24 writes");
}

fn fsync_refused_after(dev: &MemDev, ns: &vfs::MountNs, writes: u64) {
    dev.stop_after(writes);
    let r = ns.lookup("/data/big").unwrap().inode.sync(SyncKind::Fsync);
    assert!(r.is_err(), "the device stopped and fsync said {r:?}");
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

/// A cut at any write of a long run: without a journal the size on the disk stops at a step's end with the data under
/// it (and, seen from the cut, each step is one short: the bound itself); with a journal a step's mapping joins after
/// its data, and a cut with the writes after the last flush half lost and reordered still shows only written data.
#[test]
fn a_cut_during_a_long_run_finds_the_data_under_the_size() {
    let layouts: [(&str, &[&str], bool); 2] = [("bare", &["-b", "4096", "-O", "^has_journal"], false), ("journal", &["-b", "4096"], true)];
    for (name, extra, journal) in layouts {
        let s = Scratch::new();
        let (dev, ns, vol, _m) = fresh_ns(&s, "64M", extra);
        let c = Ctx::kernel(&ns);
        let want = pattern(36 * MIB + 500, 21);
        let f = c.open(None, BIG, O_WRONLY | O_CREAT, 0o644).unwrap();
        drop(f);
        fsync(&ns);
        vol.sync_fs().unwrap();
        let before = dev.image();
        dev.record();
        let f = c.open(None, BIG, O_WRONLY, 0).unwrap();
        assert_eq!(f.write(&want).unwrap(), want.len());
        drop(f);
        fsync(&ns);
        let log = dev.take_log();
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut sizes = BTreeSet::new();
        let step = (log.len() / 20).max(1);
        for k in (0..log.len()).step_by(step).chain([log.len()]) {
            let flush = log[..k].iter().rposition(|(a, _)| *a == u64::MAX).map_or(0, |i| i + 1);
            let mut img = before.clone();
            for (i, (at, d)) in log[..k].iter().enumerate() {
                // Reordering shows nothing about a journal-less disk (Linux's neither): its order is the prefix's.
                if *at == u64::MAX || (journal && i >= flush && !rng.bit()) {
                    continue;
                }
                img[*at as usize..*at as usize + d.len()].copy_from_slice(d);
            }
            let what = format!("{name}: cut after {k} of {}", log.len());
            sizes.insert(data_under_the_size(img.clone(), &want, &what));
            check_disk(&s, img, &what);
        }
        assert!(sizes.contains(&(want.len() as u64)), "{name}: never the whole file: {sizes:?}");
        if !journal {
            let steps: Vec<u64> = sizes.iter().copied().filter(|&z| z != want.len() as u64).collect();
            assert!(steps.len() >= 3 && steps.iter().all(|z| z % STEP == 0), "{name}: the size on the disk does not move by steps: {steps:?}");
        }
        // The file must also read whole through the view that wrote it.
        let f = c.open(None, BIG, O_RDONLY, 0).unwrap();
        assert_eq!(f.stat().size, want.len() as u64);
    }
}
