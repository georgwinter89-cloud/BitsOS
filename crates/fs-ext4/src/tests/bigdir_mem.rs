//! BIGDIR hardening, memory: an index is charged to the mount's pool while it is being built and given up as
//! soon as the pool refuses (not held whole and uncharged until the pass ends); the numbers kept for fstat are
//! charged too, and the ones no fstat asks about go when memory is short.

use super::bigdir::{listing, Model};
use super::bigdir_dev::{mount_counted, steam_name};
use super::util::*;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::format;
use std::sync::Arc;
use vfs::meta_shrink::{Level, MetaShrink};
use vfs::pool::Pool;

fn reg(v: &Volume, dir: u32, name: &[u8]) -> u32 {
    v.create(dir, name, 0o100644, 1000, 1000, Kind::Reg).unwrap().ino
}

#[test]
fn an_index_the_pool_refuses_is_given_up_during_its_pass() {
    let s = Scratch::new();
    let (dev, v) = mount_counted(mkfs(&s, "48M", &["-b", "4096", "-N", "8192", "-O", "^has_journal"], None));
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let mut model = Model::new();
    for i in 0..6000 {
        model.insert(steam_name(i, "m"), reg(&v, d, &steam_name(i, "m")));
    }
    v.sync_fs().unwrap();
    drop(v);
    let (dev, v) = mount_counted(dev.inner.image());
    let pool = Arc::new(Pool::new("bigdir-mem"));
    v.set_meta_pool(pool.clone());
    let blocks = v.stat(d).unwrap().size() / 4096;
    assert!(blocks >= 80, "the folder has {blocks} blocks");
    // A name in the last block: the walk that unlinks it reads every block.
    let last = {
        let st = v.lock();
        let di = st.inode(None, d).unwrap();
        model.keys().find(|n| st.find(None, &di, n).unwrap().unwrap().lblk as u64 == blocks - 1).unwrap().clone()
    };

    // The pool takes 40 KiB of this index and no more: the pass must stop near the block where that is passed
    // (about 20 KiB of index per 16 blocks here), not read the folder to its end first.
    v.lock().dirx.refuse_above = Some(40 << 10);
    let reads = v.dirx_counts().0;
    v.remove(d, &last, model.remove(&last).unwrap()).unwrap();
    let pass = v.dirx_counts().0 - reads - blocks;
    assert!(pass >= 16 && pass <= 64, "the refused pass read {pass} of {blocks} blocks before it gave up");
    assert_eq!(v.dirx_bytes(), (0, 0), "a refused index is not kept");
    assert_eq!(pool.meta_bytes(), 0, "what the pass had charged went back");
    assert!(listing(&v, d) == model);

    // Memory again, the pause over: the index is built whole and charged what it holds.
    v.lock().dirx.refuse_above = None;
    v.lock().dirx.skew_s += 11;
    model.insert(b"with-memory".to_vec(), reg(&v, d, b"with-memory"));
    assert_eq!(v.dirx_check(d), Ok(Some(model.len())));
    let (bytes, n) = v.dirx_bytes();
    let charged = pool.meta_bytes();
    assert!(n == 1 && bytes > 6000 * 20 && charged >= bytes && charged <= bytes * 5 / 4 + 1024, "{bytes} bytes held, {charged} charged");
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "after a refused pass");
}

#[test]
fn the_numbers_for_fstat_are_charged_and_the_unasked_go_when_memory_is_short() {
    let s = Scratch::new();
    let (_dev, v) = mount_counted(mkfs(&s, "32M", &["-b", "4096", "-N", "4096", "-O", "^has_journal"], None));
    let pool = Arc::new(Pool::new("bigdir-stat"));
    v.set_meta_pool(pool.clone());
    let stats = v.dir_stats();
    let mut dirs = std::vec::Vec::new();
    for k in 0..300 {
        dirs.push(v.create(ROOT, format!("d{k:03}").as_bytes(), 0o40755, 1000, 1000, Kind::Dir).unwrap().ino);
    }
    let index = pool.meta_bytes();
    for &d in &dirs {
        v.dir_stat_slow(d).unwrap();
    }
    let entry = crate::dirstat::ENTRY_BYTES;
    assert_eq!(stats.len(), 300);
    let charged = pool.meta_bytes() - index;
    assert!(charged >= 300 * entry && charged <= 300 * entry * 5 / 4 + 1024, "300 entries charged {charged} bytes");
    assert_eq!(v.lock().dirx.stat_charged(), charged);

    // Memory is short (the reclaim thread's thorough round). The first call only starts the count of who asks.
    v.shrink_meta(Level::Hard);
    assert_eq!(stats.len(), 300);
    // fstat asks about ten of them; one of the others gets a file (its numbers are kept in step, not asked for).
    for &d in &dirs[..10] {
        assert!(stats.get(d).is_some());
    }
    reg(&v, dirs[200], b"file");
    // A second call in the same second is the same call.
    v.shrink_meta(Level::Hard);
    assert_eq!(stats.len(), 300, "two calls in one second dropped what the first had just aged");
    v.lock().dirx.skew_s += 1;
    v.shrink_meta(Level::Hard);
    assert_eq!(stats.len(), 10, "the entries no fstat asked about since the last call go");
    for &d in &dirs[..10] {
        let di = v.stat(d).unwrap();
        assert_eq!(stats.get(d), Some((di.size(), di.blocks512(&v.geo))));
    }
    let left = v.lock().dirx.stat_charged();
    assert!(left >= 10 * entry && left <= 10 * entry + 1024, "{left} bytes charged for 10 entries");
    // An entry that went is made again by the next fstat, with the directory's numbers of now.
    let di = v.stat(dirs[200]).unwrap();
    assert_eq!(stats.get(dirs[200]), None);
    assert_eq!(v.dir_stat_slow(dirs[200]), Ok((di.size(), di.blocks512(&v.geo))));
    assert_eq!(stats.len(), 11);
    // An entry in the old age follows its directory's inode too.
    v.lock().dirx.skew_s += 1;
    v.shrink_meta(Level::Hard);
    let big = dirs[3];
    for i in 0..200 {
        reg(&v, big, &steam_name(i, "g"));
    }
    let di = v.stat(big).unwrap();
    assert!(di.size() > 4096);
    assert_eq!(stats.get(big), Some((di.size(), di.blocks512(&v.geo))));
    // drop_caches: everything, and the whole charge.
    v.shrink_meta(Level::All);
    assert_eq!(stats.len(), 0);
    assert_eq!(v.lock().dirx.stat_charged(), 0);
    assert_eq!(pool.meta_bytes(), 0);
}
