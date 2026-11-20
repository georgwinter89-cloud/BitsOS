//! BIGDIR: a change in a folder of 50000 names reads and writes a few blocks, not the folder. A memory device
//! that counts its reads; per change the directory blocks read and written, the device reads and writes and the
//! full walks are bounded by small constants; after each phase the index is compared with a fresh pass, a fresh
//! mount lists what a model says, and e2fsck finds nothing. The old walk (index switched off) misses the bounds.

use super::bigdir_dev::{mount_counted, steam_name, CountDev};
use super::util::*;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::collections::BTreeMap;
use std::vec::Vec;

pub(super) type Model = BTreeMap<Vec<u8>, u32>;

/// What one change cost: (directory blocks read, written, full walks, device reads, device writes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Cost {
    pub(super) dir_reads: u64,
    dir_writes: u64,
    pub(super) walks: u64,
    dev_reads: u64,
    dev_writes: u64,
}

pub(super) struct Meter<'a> {
    v: &'a Volume,
    dev: &'a CountDev,
    pub(super) worst: Cost,
    sum: Cost,
    n: u64,
}

impl<'a> Meter<'a> {
    pub(super) fn new(v: &'a Volume, dev: &'a CountDev) -> Meter<'a> {
        Meter { v, dev, worst: Cost::default(), sum: Cost::default(), n: 0 }
    }
    fn now(&self) -> Cost {
        let (r, w, s) = self.v.dirx_counts();
        Cost { dir_reads: r, dir_writes: w, walks: s, dev_reads: self.dev.reads(), dev_writes: self.dev.inner.writes() }
    }
    pub(super) fn run<T>(&mut self, f: impl FnOnce(&Volume) -> T) -> (T, Cost) {
        let a = self.now();
        let r = f(self.v);
        let b = self.now();
        let c = Cost { dir_reads: b.dir_reads - a.dir_reads, dir_writes: b.dir_writes - a.dir_writes, walks: b.walks - a.walks, dev_reads: b.dev_reads - a.dev_reads, dev_writes: b.dev_writes - a.dev_writes };
        self.worst = Cost {
            dir_reads: self.worst.dir_reads.max(c.dir_reads),
            dir_writes: self.worst.dir_writes.max(c.dir_writes),
            walks: self.worst.walks.max(c.walks),
            dev_reads: self.worst.dev_reads.max(c.dev_reads),
            dev_writes: self.worst.dev_writes.max(c.dev_writes),
        };
        self.sum.dir_reads += c.dir_reads;
        self.sum.dev_reads += c.dev_reads;
        self.sum.dev_writes += c.dev_writes;
        self.n += 1;
        (r, c)
    }
    /// The phase's bound: per change at most `dir_reads` directory blocks read (the target: 2, the ones it
    /// changes) and 2 written, no full walk, and a small constant of device requests (inode table, bitmaps,
    /// descriptors, a new directory or tree block; an unlink frees its inode in two more transactions).
    fn bounded(&mut self, what: &str, dir_reads: u64) {
        let w = self.worst;
        assert!(w.walks == 0, "{what}: a change walked the directory ({w:?})");
        assert!(w.dir_reads <= dir_reads && w.dir_writes <= 2, "{what}: directory blocks per change: {w:?}");
        let (r, wr, dr) = (self.sum.dev_reads / self.n.max(1), self.sum.dev_writes / self.n.max(1), self.sum.dir_reads as f64 / self.n.max(1) as f64);
        std::println!("bigdir {what}: {} changes; per change {dr:.2} directory blocks read, {r} device reads, {wr} device writes; worst {w:?}", self.n);
        assert!(w.dev_reads <= 40 && w.dev_writes <= 24, "{what}: device requests of the worst change: {w:?}");
        assert!(r <= 16 && wr <= 12, "{what}: device requests per change on average: {r} reads, {wr} writes");
        self.worst = Cost::default();
        self.sum = Cost::default();
        self.n = 0;
    }
}

pub(super) fn listing(v: &Volume, dir: u32) -> Model {
    v.list(dir).unwrap().into_iter().map(|(n, i, _)| (n, i)).collect()
}

/// After a phase: the index is what a fresh pass finds, a fresh mount of the image lists the model, e2fsck is
/// clean.
pub(super) fn settled(s: &Scratch, v: &Volume, dev: &CountDev, dirs: &[(u32, &Model)], what: &str) {
    for (dir, model) in dirs {
        // A wrong index heals itself (forgotten, built again): only this count shows there ever was one.
        assert_eq!(v.dirx_stales(*dir), (0, false), "{what}: an index of this volume was found stale");
        match v.dirx_check(*dir) {
            Ok(Some(n)) => assert_eq!(n, model.len(), "{what}: names in the index of {dir}"),
            // A directory of one block has none (`dirx::walked`).
            Ok(None) => assert!(v.stat(*dir).unwrap().size() <= v.geo.bs, "{what}: directory {dir} lost its index"),
            Err(e) => panic!("{what}: {e}"),
        }
    }
    v.sync_fs().unwrap();
    let img = dev.inner.image();
    let (_, again) = mount(img.clone());
    for (dir, model) in dirs {
        let got = listing(&again, *dir);
        assert!(&got == *model, "{what}: a fresh mount lists {} names in {dir}, the model has {}", got.len(), model.len());
    }
    assert_clean(s, &img, what);
}

pub(super) fn make(m: &mut Meter, model: &mut Model, dir: u32, name: Vec<u8>) {
    let (d, _) = m.run(|v| v.create(dir, &name, 0o100644, 1000, 1000, Kind::Reg).unwrap());
    assert!(model.insert(name, d.ino).is_none());
}

const BIG: usize = 50_000;
const SECOND: usize = 12_000;

#[test]
fn fifty_thousand_names_cost_a_few_blocks_per_change() {
    let s = Scratch::new();
    let img = mkfs(&s, "160M", &["-b", "4096", "-N", "80000", "-O", "^has_journal"], None);
    let (dev, v) = mount_counted(img);
    let a = v.create(ROOT, b"SHARED_DATA", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let b = v.create(ROOT, b"installed", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let (mut ma, mut mb) = (Model::new(), Model::new());
    {
        // The fill itself: every create from the empty folder on stays within the bound.
        let mut m = Meter::new(&v, &dev);
        for i in 0..BIG {
            make(&mut m, &mut ma, a, steam_name(i, "a"));
        }
        for i in 0..SECOND {
            make(&mut m, &mut mb, b, steam_name(i, "b"));
        }
        m.bounded("the fill", 2);
    }
    let blocks = (v.stat(a).unwrap().size() / 4096) as usize;
    assert!(blocks > 550, "the big folder has {blocks} blocks");
    settled(&s, &v, &dev, &[(a, &ma), (b, &mb)], "after the fill");
    drop(v);

    // A fresh mount knows nothing: the first change of each folder builds its index by one pass.
    let (dev, v) = mount_counted(dev.inner.image());
    assert_eq!(v.dir_stat_slow(a).unwrap().0, blocks as u64 * 4096);
    let mut m = Meter::new(&v, &dev);
    let t = std::time::Instant::now();
    let (_, c) = m.run(|v| v.create(a, b"first-after-mount", 0o100644, 1000, 1000, Kind::Reg).unwrap().ino);
    std::println!("bigdir the pass that builds the index of {} names in {blocks} blocks: {} us, {:?} (bytes, indexes) held", ma.len(), t.elapsed().as_micros(), v.dirx_bytes());
    ma.insert(b"first-after-mount".to_vec(), lookup_in(&v, a, b"first-after-mount"));
    assert!(c.dir_reads as usize >= blocks && c.dir_reads as usize <= blocks + 2, "the pass that builds the index read {} of {blocks} blocks", c.dir_reads);
    let (_, c) = m.run(|v| v.create(b, b"first-after-mount", 0o100644, 1000, 1000, Kind::Reg).unwrap().ino);
    mb.insert(b"first-after-mount".to_vec(), lookup_in(&v, b, b"first-after-mount"));
    assert!(c.walks == 0 && c.dir_reads > 100);
    let mut m = Meter::new(&v, &dev);

    for i in 0..1000 {
        make(&mut m, &mut ma, a, steam_name(BIG + i, "a"));
    }
    m.bounded("1000 creates", 2);
    settled(&s, &v, &dev, &[(a, &ma), (b, &mb)], "after 1000 creates");

    for i in 0..1000 {
        let name = steam_name(BIG / 2 - 500 + i * 7, "a");
        let ino = ma.remove(&name).unwrap();
        m.run(|v| v.remove(a, &name, ino).unwrap());
    }
    m.bounded("1000 unlinks from the middle", 2);
    settled(&s, &v, &dev, &[(a, &ma), (b, &mb)], "after 1000 unlinks");

    for i in 0..1000 {
        let (old, new) = (steam_name(100 + i * 20, "a"), steam_name(i, "renamed-in-place-"));
        let ino = ma.remove(&old).unwrap();
        m.run(|v| v.rename(a, &old, ino, a, &new, None).unwrap());
        ma.insert(new, ino);
    }
    m.bounded("1000 renames in place", 2);
    settled(&s, &v, &dev, &[(a, &ma), (b, &mb)], "after 1000 renames in place");

    for i in 0..1000 {
        let (old, new) = (steam_name(40_000 + i * 3, "a"), steam_name(i, "moved-"));
        let ino = ma.remove(&old).unwrap();
        m.run(|v| v.rename(a, &old, ino, b, &new, None).unwrap());
        mb.insert(new, ino);
    }
    // Over existing names too (the target's entry is pointed at the moved file).
    for i in 0..200 {
        let (old, new) = (steam_name(35_000 + i * 3, "a"), steam_name(i * 5, "b"));
        let ino = ma.remove(&old).unwrap();
        let over = mb.insert(new.clone(), ino).unwrap();
        m.run(|v| v.rename(a, &old, ino, b, &new, Some(over)).unwrap());
    }
    m.bounded("1200 renames into a second big folder", 2);
    settled(&s, &v, &dev, &[(a, &ma), (b, &mb)], "after the renames into the second folder");

    let mut subs = Vec::new();
    for i in 0..200 {
        let name = steam_name(i, "Folder-");
        let (d, _) = m.run(|v| v.create(a, &name, 0o40755, 1000, 1000, Kind::Dir).unwrap());
        ma.insert(name.clone(), d.ino);
        subs.push((name, d.ino));
    }
    let inner = v.create(subs[0].1, b"inside", 0o100644, 1000, 1000, Kind::Reg).unwrap().ino;
    assert_eq!(v.remove(a, &subs[0].0, subs[0].1).err(), Some(crate::errno::ENOTEMPTY));
    v.remove(subs[0].1, b"inside", inner).unwrap();
    for (name, ino) in &subs {
        m.run(|v| v.remove(a, name, *ino).unwrap());
        ma.remove(name);
    }
    m.bounded("200 mkdir and rmdir", 2);
    settled(&s, &v, &dev, &[(a, &ma), (b, &mb)], "after mkdir and rmdir");

    // fstat's numbers followed every change of the directory's inode (`dirx_check` compared them above).
    let d = v.stat(a).unwrap();
    assert_eq!(v.dir_stats().get(a), Some((d.size(), d.blocks512(&v.geo))));

    // The walk before BIGDIR (index off) misses every bound: each change reads the folder.
    v.dirx_off(true);
    let mut old = Meter::new(&v, &dev);
    // Names too long for the gaps the phases above left (as in Steam's growing folder, where no block has room):
    // the old create tries every block, the old rename and unlink walk to the name at the end.
    let (name, name2) = ([b'W'; 200], [b'X'; 200]);
    let (d, c1) = old.run(|v| v.create(a, &name, 0o100644, 1000, 1000, Kind::Reg).unwrap());
    let (_, c2) = old.run(|v| v.rename(a, &name, d.ino, a, &name2, None).unwrap());
    let (_, c3) = old.run(|v| v.remove(a, &name2, d.ino).unwrap());
    for (what, c) in [("create", c1), ("rename", c2), ("unlink", c3)] {
        assert!(c.walks >= 1 && c.dir_reads as usize >= blocks, "the old {what} did not walk the folder of {blocks} blocks: {c:?}");
        std::println!("bigdir the walk before BIGDIR, one {what}: {c:?}");
    }
    v.dirx_off(false);
    // The same three with the index: the bound holds (the first builds the index again, outside the meter).
    make(&mut Meter::new(&v, &dev), &mut ma, a, steam_name(1, "after-the-walks-"));
    make(&mut Meter::new(&v, &dev), &mut mb, b, steam_name(1, "after-the-walks-"));
    let (d, _) = m.run(|v| v.create(a, &name, 0o100644, 1000, 1000, Kind::Reg).unwrap());
    m.run(|v| v.rename(a, &name, d.ino, a, &name2, None).unwrap());
    m.run(|v| v.remove(a, &name2, d.ino).unwrap());
    m.bounded("the same three changes with the index", 2);
    settled(&s, &v, &dev, &[(a, &ma), (b, &mb)], "after the old walk");
}

fn lookup_in(v: &Volume, dir: u32, name: &[u8]) -> u32 {
    v.list(dir).unwrap().into_iter().find(|(n, _, _)| n == name).unwrap().1
}
