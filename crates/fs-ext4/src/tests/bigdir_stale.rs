//! BIGDIR hardening: an index that is wrong (made so on purpose through `Volume::dirx_corrupt`; no code path is
//! known to make one) is never believed where believing it would remove or duplicate something. rmdir and a
//! rename over a directory read "empty" from the blocks; unlink, rename and exchange find a name the index
//! lost; a create does not add a name twice; and every such meeting is counted (`dirx_stales`), forgets only
//! that directory's index and leaves the volume clean for e2fsck.

use super::bigdir::{listing, Model};
use super::bigdir_dev::{mount_counted, steam_name, CountDev};
use super::util::*;
use crate::dirx::Corrupt;
use crate::errno::*;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::sync::Arc;
use std::vec::Vec;

fn reg(v: &Volume, dir: u32, name: &[u8]) -> Result<u32, i64> {
    v.create(dir, name, 0o100644, 1000, 1000, Kind::Reg).map(|d| d.ino)
}

/// A volume with folder `d` of `n` files (several blocks, so it has an index) and its model.
fn folder(s: &Scratch, n: usize, journal: bool) -> (Arc<CountDev>, Arc<Volume>, u32, Model) {
    let o: &[&str] = if journal { &["-b", "4096", "-N", "4096"] } else { &["-b", "4096", "-N", "4096", "-O", "^has_journal"] };
    let (dev, v) = mount_counted(mkfs(s, "32M", o, None));
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let mut model = Model::new();
    for i in 0..n {
        let name = steam_name(i, "s");
        model.insert(name.clone(), reg(&v, d, &name).unwrap());
    }
    assert_eq!(v.dirx_check(d), Ok(Some(n)));
    (dev, v, d, model)
}

fn missing(v: &Volume, d: u32, name: &[u8]) {
    assert!(v.dirx_corrupt(d, |x| Corrupt::Missing(x.hash(name))), "nothing to take out of the index");
    assert!(v.dirx_check(d).is_err(), "the index is still right");
}

fn end(s: &Scratch, dev: &CountDev, v: &Volume, d: u32, model: &Model, what: &str) {
    assert!(listing(v, d) == *model, "{what}: the folder is not what the model says");
    // One more change builds the index again from the blocks.
    let ino = reg(v, d, b"after").unwrap();
    v.remove(d, b"after", ino).unwrap();
    assert_eq!(v.dirx_check(d), Ok(Some(model.len())), "{what}");
    assert!(!v.dirx_stales(d).1, "{what}: still a suspect after its index was built again");
    v.sync_fs().unwrap();
    assert_clean(s, &dev.inner.image(), what);
}

/// Finding 1: the index says "no names" of a folder that has 300. rmdir and a rename over it must refuse.
#[test]
fn an_index_that_counts_no_name_removes_no_folder() {
    for journal in [false, true] {
        let s = Scratch::new();
        let (dev, v, d, model) = folder(&s, 300, journal);
        let empty = v.create(ROOT, b"empty", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;

        assert!(v.dirx_corrupt(d, |_| Corrupt::Empty));
        assert_eq!(v.remove(ROOT, b"d", d).err(), Some(ENOTEMPTY), "rmdir of a folder whose index counts no name");
        assert_eq!(v.dirx_stales(d), (1, true));
        assert_eq!(v.dirx_check(d), Ok(None), "the stale index is forgotten");

        // The same for a rename over it; the index is built again by a change first.
        let ino = reg(&v, d, b"again").unwrap();
        v.remove(d, b"again", ino).unwrap();
        assert_eq!(v.dirx_check(d), Ok(Some(300)));
        assert!(v.dirx_corrupt(d, |_| Corrupt::Empty));
        assert_eq!(v.rename(ROOT, b"empty", empty, ROOT, b"d", Some(d)).err(), Some(ENOTEMPTY), "a rename over a folder whose index counts no name");
        assert_eq!(v.dirx_stales(d).0, 2);
        assert_eq!(listing(&v, ROOT).len(), 3, "d, empty and lost+found");
        end(&s, &dev, &v, d, &model, "an index that counted no name");

        // An index that is right still answers "not empty" without a block, and an emptied folder goes.
        let reads = v.dirx_counts().0;
        assert_eq!(v.remove(ROOT, b"d", d).err(), Some(ENOTEMPTY));
        assert!(v.dirx_counts().0 - reads <= 1, "a right index answers not-empty without walking the folder");
        for (name, ino) in &model {
            v.remove(d, name, *ino).unwrap();
        }
        v.remove(ROOT, b"d", d).unwrap();
        assert_eq!(v.dirx_stales(d).0, 2, "no more stale indexes on the way");
        v.sync_fs().unwrap();
        assert_clean(&s, &dev.inner.image(), "the emptied folder removed");
    }
}

/// Finding 2: names the index lost are found by the walk that confirms a miss: unlink, rename (the old name and
/// the name it replaces), exchange. A name that really is not there costs that one walk and answers ENOENT.
#[test]
fn a_name_the_index_lost_is_still_unlinked_renamed_and_exchanged() {
    let s = Scratch::new();
    let (dev, v, d, mut model) = folder(&s, 400, true);
    let other = v.create(ROOT, b"other", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let free = reg(&v, other, b"free").unwrap();
    let name = |i: usize| steam_name(i, "s");
    let mut stale = 0;
    let mut hit = |v: &Volume, what: &str| {
        stale += 1;
        assert_eq!(v.dirx_stales(d).0, stale, "{what}: the stale index was not counted");
        // The next change builds the index again.
        let ino = reg(v, d, b"between").unwrap();
        v.remove(d, b"between", ino).unwrap();
    };

    missing(&v, d, &name(10));
    v.remove(d, &name(10), model.remove(&name(10)).unwrap()).unwrap();
    hit(&v, "unlink");

    missing(&v, d, &name(20));
    let ino = model.remove(&name(20)).unwrap();
    v.rename(d, &name(20), ino, d, b"renamed", None).unwrap();
    model.insert(b"renamed".to_vec(), ino);
    hit(&v, "rename, the old name");

    missing(&v, d, &name(30));
    let over = model.insert(name(30), free).unwrap();
    v.rename(other, b"free", free, d, &name(30), Some(over)).unwrap();
    hit(&v, "rename, the name it replaces");

    missing(&v, d, &name(40));
    let (a, b) = (model[&name(40)], model[&name(41)]);
    v.exchange(d, &name(40), a, d, &name(41), b).unwrap();
    model.insert(name(40), b);
    model.insert(name(41), a);
    hit(&v, "exchange");

    // A place that is off by a few bytes: the entry there is not the name's, the walk finds it.
    assert!(v.dirx_corrupt(d, |x| Corrupt::Offset(x.hash(&name(50)))));
    v.remove(d, &name(50), model.remove(&name(50)).unwrap()).unwrap();
    hit(&v, "unlink through a wrong offset");

    // A name that is not there: ENOENT after one walk of the folder, nothing stale.
    assert_eq!(v.dirx_check(d), Ok(Some(model.len())));
    let (reads, _, walks) = v.dirx_counts();
    let blocks = v.stat(d).unwrap().size() / 4096;
    assert_eq!(v.remove(d, b"never-there", free).err(), Some(ENOENT));
    let (reads2, _, walks2) = v.dirx_counts();
    assert_eq!((reads2 - reads, walks2 - walks), (blocks, 1), "a confirmed miss costs one walk of the folder's {blocks} blocks");
    assert_eq!(v.dirx_stales(d), (stale, false));
    assert_eq!(v.dirx_check(d), Ok(Some(model.len())), "a confirmed miss keeps the index");
    end(&s, &dev, &v, d, &model, "names the index had lost");
}

/// Finding 2, the add: a create never writes a name the folder has a second time. A wrong offset is seen by the
/// create itself; a folder whose index was found stale and cannot be built again (no memory) gets the look at
/// every block before each add; and through the VFS a name the index lost never reaches ext4 (the VFS knows it).
#[test]
fn a_create_adds_no_name_twice_beside_a_stale_index() {
    let s = Scratch::new();
    let (dev, v, d, mut model) = folder(&s, 400, false);
    let name = |i: usize| steam_name(i, "s");
    let twice = |v: &Volume, n: &[u8]| v.list(d).unwrap().iter().filter(|e| e.0 == n).count();

    assert!(v.dirx_corrupt(d, |x| Corrupt::Offset(x.hash(&name(5)))));
    assert_eq!(reg(&v, d, &name(5)).err(), Some(EEXIST), "a create of a name whose place in the index is wrong");
    assert_eq!(v.dirx_stales(d), (1, true));

    // No memory for a new index: the folder stays a suspect, and every add walks it for the name first.
    v.dirx_refuse(true);
    for i in [6, 399] {
        assert_eq!(reg(&v, d, &name(i)).err(), Some(EEXIST), "a create in a suspect folder of a name it has");
        assert_eq!(v.link(d, &name(i), model[&name(0)]).err(), Some(EEXIST));
        assert_eq!(twice(&v, &name(i)), 1);
    }
    model.insert(b"new-while-suspect".to_vec(), reg(&v, d, b"new-while-suspect").unwrap());
    assert_eq!(v.dirx_stales(d), (1, true));
    assert_eq!(v.dirx_check(d), Ok(None));
    // Memory again (and the pause after the refusal over): the next change builds the index, the suspicion ends.
    v.dirx_refuse(false);
    v.lock().dirx.skew_s += 11;
    model.insert(b"new-with-an-index".to_vec(), reg(&v, d, b"new-with-an-index").unwrap());
    assert_eq!(v.dirx_stales(d), (1, false));
    assert_eq!(v.dirx_check(d), Ok(Some(model.len())));
    assert!(listing(&v, d) == model);
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "creates beside a stale index");
    drop(v);

    // Through the VFS: the folder is loaded, a name the index lost is refused by the VFS's own map.
    let dev2 = CountDev::new(dev.inner.image());
    let ns = Arc::new(vfs::MountNs::new_empty());
    let (v2, _m) = crate::mount::mount_dev(dev2.clone(), None, &ns, "/data", opts()).unwrap();
    let c = vfs::ops::Ctx::kernel(&ns);
    use vfs::abi::{O_CREAT, O_EXCL, O_RDWR};
    drop(c.open(None, b"/data/d/first-change", O_RDWR | O_CREAT, 0o644).unwrap());
    let lost = name(77);
    let path: Vec<u8> = [b"/data/d/".as_slice(), &lost].concat();
    assert!(v2.dirx_corrupt(d, |x| Corrupt::Missing(x.hash(&lost))));
    assert_eq!(c.open(None, &path, O_RDWR | O_CREAT | O_EXCL, 0o644).err(), Some(EEXIST));
    assert_eq!(c.mkdir(None, &path, 0o755).err(), Some(EEXIST));
    assert_eq!(v2.list(d).unwrap().iter().filter(|e| e.0 == lost).count(), 1);
    // And the VFS's unlink of it reaches ext4, which finds it by the walk.
    c.unlink(None, &path).unwrap();
    assert_eq!(v2.dirx_stales(d).0, 1);
    assert_eq!(v2.list(d).unwrap().iter().filter(|e| e.0 == lost).count(), 0);
    v2.sync_fs().unwrap();
    assert_clean(&s, &dev2.inner.image(), "a lost name through the VFS");
}

/// Finding 3: an index that knows fewer blocks than the folder has cannot follow a change in the last block. Only
/// that index goes, counted as stale; nothing is refused and no other folder's index building pauses.
#[test]
fn an_index_out_of_step_goes_alone_and_pauses_nothing() {
    let s = Scratch::new();
    let (dev, v, d, mut model) = folder(&s, 300, true);
    let e = v.create(ROOT, b"e", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let mut me = Model::new();
    for i in 0..150 {
        me.insert(steam_name(i, "e"), reg(&v, e, &steam_name(i, "e")).unwrap());
    }
    v.lock().dirx.map.clear();
    let ino = reg(&v, d, b"build").unwrap();
    v.remove(d, b"build", ino).unwrap();
    // A name in the folder's last block.
    let last = {
        let st = v.lock();
        let di = st.inode(None, d).unwrap();
        let end = st.dir_blocks(&di).unwrap() - 1;
        model.keys().find(|n| st.find(None, &di, n).unwrap().unwrap().lblk == end).unwrap().clone()
    };
    assert!(v.dirx_corrupt(d, |_| Corrupt::ShortRooms));
    v.remove(d, &last, model.remove(&last).unwrap()).unwrap();
    assert_eq!(v.dirx_stales(d), (1, true), "the index that could not follow counts as stale");
    assert_eq!(v.dirx_check(d), Ok(None));
    // The other folder builds its index right away: nothing paused.
    let ino = reg(&v, e, b"now").unwrap();
    v.remove(e, b"now", ino).unwrap();
    assert_eq!(v.dirx_check(e), Ok(Some(me.len())), "another folder's index was not built: building paused");
    let walks = v.dirx_counts().2;
    model.insert(b"next".to_vec(), reg(&v, d, b"next").unwrap());
    assert_eq!(v.dirx_counts().2, walks, "the folder's next change walked: its index was not built again");
    assert_eq!(v.dirx_stales(d), (1, false));
    assert_eq!(v.dirx_check(d), Ok(Some(model.len())));
    assert!(listing(&v, e) == me);
    end(&s, &dev, &v, d, &model, "an index out of step");
}
