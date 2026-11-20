//! BIGDIR: a change that fails leaves the index and the disk equal. The index is compared with a fresh pass
//! over the directory (`Volume::dirx_check`) after every failure: no room for a new directory block, a name
//! that exists, a bad name, a device error at each read of each kind of change, a failed write.

use super::bigdir_dev::{mount_counted, steam_name, CountDev};
use super::util::*;
use crate::errno::*;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::collections::BTreeMap;
use std::format;
use std::vec::Vec;

type Model = BTreeMap<Vec<u8>, u32>;

fn reg(v: &Volume, dir: u32, name: &[u8]) -> Result<u32, i64> {
    v.create(dir, name, 0o100644, 1000, 1000, Kind::Reg).map(|d| d.ino)
}

fn listing(v: &Volume, dir: u32) -> Model {
    v.list(dir).unwrap().into_iter().map(|(n, i, _)| (n, i)).collect()
}

/// The index of `dir` is there and holds exactly the model.
fn exact(v: &Volume, dir: u32, model: &Model, what: &str) {
    assert_eq!(v.dirx_check(dir), Ok(Some(model.len())), "{what}");
    assert_eq!(v.dirx_stales(dir), (0, false), "{what}: an index was found stale");
    assert!(listing(v, dir) == *model, "{what}: the directory is not what the model says");
}

#[test]
fn refused_changes_keep_the_index() {
    let s = Scratch::new();
    let (dev, v) = mount_counted(mkfs(&s, "8M", &["-b", "1024", "-N", "12000", "-O", "^has_journal"], None));
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let other = v.create(ROOT, b"other", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let mut model = Model::new();
    for i in 0..600 {
        let n = steam_name(i, "k");
        model.insert(n.clone(), reg(&v, d, &n).unwrap());
    }
    v.dir_stat_slow(d).unwrap();
    let have = steam_name(7, "k");
    let free = reg(&v, other, b"free").unwrap();

    assert_eq!(reg(&v, d, &[b'x'; 256]).err(), Some(ENAMETOOLONG));
    assert_eq!(reg(&v, d, b"a/b").err(), Some(EINVAL));
    assert_eq!(reg(&v, d, &have).err(), Some(EEXIST), "a create of a name the directory has");
    assert_eq!(v.create(d, &have, 0o40755, 1000, 1000, Kind::Dir).err(), Some(EEXIST));
    assert_eq!(v.link(d, &have, free).err(), Some(EEXIST));
    assert_eq!(v.rename(other, b"free", free, d, &have, None).err(), Some(EEXIST), "a rename onto a name without its target");
    assert_eq!(v.remove(d, b"never-there", free).err(), Some(ENOENT));
    assert_eq!(v.remove(d, &have, free).err(), Some(ENOENT), "the name with another inode");
    assert_eq!(v.rename(d, b"never-there", free, d, b"x", None).err(), Some(ENOENT));
    exact(&v, d, &model, "after refused changes");
    assert_eq!(listing(&v, other).len(), 1);

    // No room for a new directory block: slow symlinks (a block each) take every block a user may have.
    let fill = v.create(ROOT, b"fill", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let target = [b't'; 200];
    let mut links = 0;
    loop {
        match v.create(fill, format!("l{links}").as_bytes(), 0o120777, 1000, 1000, Kind::Symlink(&target)) {
            Ok(_) => links += 1,
            Err(e) => {
                assert_eq!(e, ENOSPC);
                break;
            }
        }
    }
    assert!(links > 1000, "{links} symlinks filled the volume");
    let mut made = 0;
    let refused = loop {
        let n = steam_name(made, "after-full-");
        match reg(&v, d, &n) {
            Ok(i) => {
                model.insert(n, i);
                made += 1;
                assert!(made < 500, "the full volume never refused a directory block");
            }
            Err(e) => break e,
        }
    };
    assert_eq!(refused, ENOSPC);
    for k in 0..3 {
        assert_eq!(reg(&v, d, &steam_name(k, "refused-")).err(), Some(ENOSPC));
        assert_eq!(v.rename(other, b"free", free, d, &steam_name(k, "refused-move-"), None).err(), Some(ENOSPC));
        exact(&v, d, &model, "after ENOSPC for a new directory block");
    }
    // fstat's numbers did not move with the refused block.
    let di = v.stat(d).unwrap();
    assert_eq!(v.dir_stats().get(d), Some((di.size(), di.blocks512(&v.geo))));
    // Room again: the same create goes through, into a new block the index now knows.
    for k in 0..40 {
        v.remove(fill, format!("l{k}").as_bytes(), listing(&v, fill)[format!("l{k}").as_bytes()]).unwrap();
    }
    for k in 0..30 {
        let n = steam_name(k, "refused-");
        model.insert(n.clone(), reg(&v, d, &n).unwrap());
    }
    exact(&v, d, &model, "after room came back");
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "refused changes");
}

/// One change of each kind, with the `n`-th device read failing, for every n until the change needs fewer.
#[test]
fn a_device_error_at_any_read_leaves_the_index_as_the_disk() {
    let s = Scratch::new();
    let (dev, v) = mount_counted(mkfs(&s, "32M", &["-b", "4096", "-N", "8192", "-O", "^has_journal"], None));
    let a = v.create(ROOT, b"a", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let b = v.create(ROOT, b"b", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let (mut ma, mut mb) = (Model::new(), Model::new());
    for i in 0..2000 {
        let n = steam_name(i, "a");
        ma.insert(n.clone(), reg(&v, a, &n).unwrap());
    }
    for i in 0..700 {
        let n = steam_name(i, "b");
        mb.insert(n.clone(), reg(&v, b, &n).unwrap());
    }
    let mut next = 0usize;
    let mut failed = 0;
    let mut kept = 0;
    for kind in 0..8 {
        for n in 1..200i64 {
            next += 1;
            let pick = |m: &Model, k: usize| m.iter().nth(k * 37 % m.len()).map(|(n, i)| (n.clone(), *i)).unwrap();
            let (va, ia) = pick(&ma, next);
            let (vb, ib) = pick(&mb, next);
            let new = steam_name(next, "new-");
            dev.fail_read(n);
            // Each arm returns what the models become when it worked.
            let r: Result<(), i64> = match kind {
                0 => reg(&v, a, &new).map(|i| {
                    ma.insert(new.clone(), i);
                }),
                1 => v.remove(a, &va, ia).map(|()| {
                    ma.remove(&va);
                }),
                2 => v.rename(a, &va, ia, a, &new, None).map(|()| {
                    ma.remove(&va);
                    ma.insert(new.clone(), ia);
                }),
                3 => v.rename(a, &va, ia, b, &new, None).map(|()| {
                    ma.remove(&va);
                    mb.insert(new.clone(), ia);
                }),
                4 => v.rename(a, &va, ia, b, &vb, Some(ib)).map(|()| {
                    ma.remove(&va);
                    mb.insert(vb.clone(), ia);
                }),
                5 => v.exchange(a, &va, ia, b, &vb, ib).map(|()| {
                    ma.insert(va.clone(), ib);
                    mb.insert(vb.clone(), ia);
                }),
                6 => v.create(a, &new, 0o40755, 0, 0, Kind::Dir).map(|d| {
                    ma.insert(new.clone(), d.ino);
                }),
                _ => v.link(b, &new, ia).map(|()| {
                    mb.insert(new.clone(), ia);
                }),
            };
            let unused = dev.fail_read_left() > 0;
            dev.fail_read(0);
            assert!(v.writable(), "kind {kind}, read {n}: a failed read stopped the volume");
            for (dir, m) in [(a, &ma), (b, &mb)] {
                match v.dirx_check(dir) {
                    Ok(Some(k)) => {
                        assert_eq!(k, m.len(), "kind {kind}, read {n} failed ({r:?}): names in the index of {dir}");
                        kept += 1;
                    }
                    Ok(None) => {}
                    Err(e) => panic!("kind {kind}, read {n} failed ({r:?}): {e}"),
                }
                assert!(listing(&v, dir) == *m, "kind {kind}, read {n} failed ({r:?}): directory {dir} is not what the model says");
            }
            match r {
                Err(e) => {
                    assert_eq!(e, EIO, "kind {kind}, read {n}");
                    failed += 1;
                }
                Ok(()) => assert!(unused, "kind {kind}: the change worked although its read {n} failed"),
            }
            if unused {
                break;
            }
        }
    }
    assert!(failed > 40 && kept > 40, "{failed} changes failed, the index was kept {kept} times");
    assert_eq!(v.dirx_stales(a).0, 0, "a failed change left an index that was later found stale");
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "device errors at reads");
}

#[test]
fn a_failed_write_forgets_every_index() {
    let s = Scratch::new();
    let (dev, v): (std::sync::Arc<CountDev>, _) = mount_counted(mkfs(&s, "16M", &["-O", "^has_journal"], None));
    let d = v.create(ROOT, b"d", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    for i in 0..300 {
        reg(&v, d, &steam_name(i, "w")).unwrap();
    }
    v.dir_stat_slow(d).unwrap();
    assert_eq!(v.dirx_bytes().1, 1, "the folder's index (the root has one block and none)");
    dev.fail_write(2);
    assert!(reg(&v, d, b"lost").is_err());
    assert!(!v.writable());
    assert_eq!(v.dirx_bytes().1, 0, "memory may be ahead of the disk: nothing of it is kept");
    assert_eq!(v.dir_stats().get(d), None);
    assert_eq!(v.dirx_check(d), Ok(None));
}

/// The pool refuses the index's memory: the index is forgotten, changes walk the directory and stay right; after
/// a pause the index is built again.
#[test]
fn a_refused_charge_falls_back_to_the_walk() {
    let s = Scratch::new();
    let (dev, v) = mount_counted(mkfs(&s, "16M", &["-N", "8192"], None));
    let d = v.create(ROOT, b"d", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let mut model = Model::new();
    for i in 0..500 {
        let n = steam_name(i, "p");
        model.insert(n.clone(), reg(&v, d, &n).unwrap());
    }
    exact(&v, d, &model, "before the refusal");
    v.dirx_refuse(true);
    let walks = v.dirx_counts().2;
    for i in 500..900 {
        let n = steam_name(i, "p");
        model.insert(n.clone(), reg(&v, d, &n).unwrap());
        if i % 3 == 0 {
            let gone = steam_name(i - 400, "p");
            v.remove(d, &gone, model.remove(&gone).unwrap()).unwrap();
        }
    }
    assert_eq!(v.dirx_check(d), Ok(None), "an index the pool refused is forgotten");
    assert!(v.dirx_counts().2 - walks >= 400, "the changes walked the directory");
    assert!(listing(&v, d) == model);
    v.dirx_refuse(false);
    // The pause is in seconds since boot, whatever the time of day does.
    let again = |v: &Volume, k: usize| {
        let n = steam_name(k, "later-");
        let ino = reg(v, d, &n).unwrap();
        v.remove(d, &n, ino).unwrap();
    };
    again(&v, 0);
    assert_eq!(v.dirx_check(d), Ok(None), "no index is built within the pause");
    v.lock().dirx.skew_s += 9;
    again(&v, 1);
    assert_eq!(v.dirx_check(d), Ok(None), "no index is built within the pause");
    v.lock().dirx.skew_s += 2;
    again(&v, 2);
    exact(&v, d, &model, "after the pause");
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "a refused index");
}

/// Memory pressure: the indexes nobody used for a while go, longest unused first; the next change builds anew.
#[test]
fn unused_indexes_are_given_back() {
    use vfs::meta_shrink::{Level, MetaShrink};
    let s = Scratch::new();
    let (_dev, v) = mount_counted(mkfs(&s, "16M", &["-N", "8192"], None));
    let mut dirs = Vec::new();
    for k in 0..4 {
        let d = v.create(ROOT, format!("d{k}").as_bytes(), 0o40755, 0, 0, Kind::Dir).unwrap().ino;
        for i in 0..200 {
            reg(&v, d, &steam_name(i, "s")).unwrap();
        }
        dirs.push(d);
    }
    let (bytes, n) = v.dirx_bytes();
    assert!(n == 4 && bytes > 4 * 200 * 20, "{n} indexes of {bytes} bytes");
    let now = v.lock().dirx.now_s();
    assert_eq!(v.lock().dirx.shrink(now, 1_000_000), 0, "nothing is that long unused");
    assert_eq!(v.dirx_bytes().1, 4);
    // One folder is used 60 s later: a call for what was unused for 50 s takes the other three.
    v.lock().dirx.skew_s += 60;
    reg(&v, dirs[2], b"used-later").unwrap();
    let now = v.lock().dirx.now_s();
    let took = v.lock().dirx.shrink(now, 50);
    assert!(took >= bytes / 2, "{took} of {bytes} bytes given back");
    assert_eq!(v.dirx_bytes().1, 1);
    assert_eq!(v.dirx_check(dirs[2]), Ok(Some(201)));
    assert_eq!(v.lock().dirx.shrink(now, 50), 0, "asked again within the second: nothing");
    assert!(v.lock().dirx.shrink(now + 100, 50) > 0, "the last one goes once it is unused that long");
    assert_eq!(v.dirx_bytes(), (0, 0));
    reg(&v, dirs[1], b"again").unwrap();
    assert_eq!(v.dirx_check(dirs[1]), Ok(Some(201)));
    // drop_caches: everything, at once.
    assert!(v.shrink_meta(Level::All) > 0);
    assert_eq!(v.dirx_bytes(), (0, 0));
    assert_eq!(v.dirx_check(dirs[1]), Ok(None));
}

/// Checker finding 1: an index charges the pool what it holds. With a fixed 64 KiB piece per index, 2000
/// one-file directories took 131 MB of the system's commit for 108 KB of indexes.
#[test]
fn the_pool_is_charged_what_the_indexes_hold() {
    let s = Scratch::new();
    let (dev, v) = mount_counted(mkfs(&s, "64M", &["-b", "4096", "-N", "16384", "-O", "^has_journal"], None));
    let pool = std::sync::Arc::new(vfs::pool::Pool::new("bigdir-test"));
    v.set_meta_pool(pool.clone());
    let top = v.create(ROOT, b"top", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    for k in 0..2000 {
        let d = v.create(top, format!("small-{k:04}").as_bytes(), 0o40755, 0, 0, Kind::Dir).unwrap().ino;
        reg(&v, d, b"one-file").unwrap();
        // A directory of one block answers as an indexed one does.
        assert_eq!(reg(&v, d, b"one-file").err(), Some(EEXIST));
    }
    // The one-block directories have no index; `top` (2000 names) has.
    let (bytes, n) = v.dirx_bytes();
    let charged = pool.meta_bytes();
    assert_eq!(n, 1, "{n} indexes after 2000 small directories");
    assert!(bytes > 2000 * 20 && charged >= bytes && charged <= bytes * 5 / 4 + 1024, "{bytes} bytes held, {charged} charged");
    assert!(charged < 128 << 10, "2000 small directories charged {charged} bytes");

    // Directories of a few blocks: each index is charged what it holds, at most a quarter (or 1 KiB) ahead.
    let mut mids = Vec::new();
    for k in 0..40 {
        let d = v.create(ROOT, format!("mid-{k}").as_bytes(), 0o40755, 0, 0, Kind::Dir).unwrap().ino;
        for i in 0..200 {
            reg(&v, d, &steam_name(i, "m")).unwrap();
        }
        assert!(v.dirx_check(d).unwrap().is_some());
        mids.push(d);
    }
    let (bytes, n) = v.dirx_bytes();
    let charged = pool.meta_bytes();
    assert_eq!(n, 41);
    assert!(charged >= bytes && charged <= bytes * 5 / 4 + n as u64 * 1024, "{n} indexes hold {bytes} bytes, {charged} charged");
    assert!(charged < 400 << 10, "41 indexes charged {charged} bytes");
    // The charge goes back with the indexes.
    assert!(vfs::meta_shrink::MetaShrink::shrink_meta(&*v, vfs::meta_shrink::Level::All) >= bytes);
    assert_eq!(pool.meta_bytes(), 0);
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "small and middle directories");
}

/// A directory's index starts when it gets its second block, from the one block the change read anyway: no
/// change on the way from an empty directory to a large one reads more than two directory blocks.
#[test]
fn the_index_starts_with_the_second_block() {
    let s = Scratch::new();
    let (dev, v) = mount_counted(mkfs(&s, "16M", &["-b", "4096", "-N", "4096", "-O", "^has_journal"], None));
    let d = v.create(ROOT, b"d", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let mut model = Model::new();
    let mut worst = 0;
    for i in 0..400 {
        let before = v.dirx_counts();
        let n = steam_name(i, "g");
        model.insert(n.clone(), reg(&v, d, &n).unwrap());
        let after = v.dirx_counts();
        worst = worst.max(after.0 - before.0);
        assert_eq!(after.2, before.2, "create {i} walked the directory");
        let blocks = v.stat(d).unwrap().size() / 4096;
        assert_eq!(v.dirx_check(d).unwrap().is_some(), blocks > 1, "create {i}: {blocks} blocks");
    }
    assert!(worst <= 2, "a create read {worst} directory blocks");
    exact(&v, d, &model, "after growing from one block");
    // Renames and unlinks in a one-block directory: the walk of its block, counted as no walk.
    let small = v.create(ROOT, b"small", 0o40755, 0, 0, Kind::Dir).unwrap().ino;
    let a = reg(&v, small, b"a").unwrap();
    let before = v.dirx_counts();
    v.rename(small, b"a", a, small, b"b", None).unwrap();
    assert_eq!(v.rename(small, b"b", a, small, b"b", None).err(), Some(EEXIST));
    v.remove(small, b"b", a).unwrap();
    v.remove(ROOT, b"small", small).unwrap();
    let after = v.dirx_counts();
    assert_eq!(after.2, before.2);
    assert_eq!(v.dirx_check(small), Ok(None));
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "a directory growing past one block");
}
