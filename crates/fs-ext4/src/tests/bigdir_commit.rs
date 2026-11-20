//! BIGDIR hardening: a transaction that changed an indexed folder and is not committed leaves no index ahead of
//! the disk, on each way `State::commit` gives a transaction up on a volume with a journal: the device timing
//! out under the commit's first write after a sync, a full log whose commit in the middle of the operation times
//! out or finds no memory, a frozen volume. After each the operation has failed, the volume is writable, the
//! index is what a fresh pass builds and the folder is the model. And the guard for a new such way: a
//! transaction dropped with a directory in it trips an assertion.
//!
//! The changes run under the plain volume lock (`Volume::lock`, as freeing an inode and a write-back step's end
//! do): `Volume::create` waits for the thaw and marks the volume dirty before its transaction begins, so it
//! cannot reach these branches by itself.

use super::bigdir::{listing, Model};
use super::bigdir_dev::steam_name;
use super::util::*;
use crate::errno::*;
use crate::inode::ROOT;
use crate::state::State;
use crate::tx::Tx;
use crate::{Kind, MemDev, Volume};
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// A hard link `name` to `ino` in folder `d`, as `Volume::link` makes it, in one transaction of its own.
fn link(st: &mut State, d: u32, name: &[u8], ino: u32) -> Result<(), i64> {
    st.with_tx(|st, tx| {
        let mut parent = st.inode(Some(tx), d)?;
        let mut t = st.inode(Some(tx), ino)?;
        let keep = st.keep_for(None);
        st.add_name(tx, &mut parent, name, ino, crate::dir::FT_REG, keep)?;
        t.set_links(t.links() + 1);
        st.put_inode(tx, &mut t)?;
        st.put_inode(tx, &mut parent)
    })
}

struct Fx {
    s: Scratch,
    dev: Arc<MemDev>,
    v: Arc<Volume>,
    d: u32,
    file: u32,
    model: Model,
}

/// A journal volume with folder `d` of 300 names (indexed) and a file to link into it.
fn fixture() -> Fx {
    let s = Scratch::new();
    let (dev, v) = mount(mkfs(&s, "32M", &["-b", "4096", "-N", "4096"], None));
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let mut model = Model::new();
    for i in 0..300 {
        let n = steam_name(i, "c");
        model.insert(n.clone(), v.create(d, &n, 0o100644, 1000, 1000, Kind::Reg).unwrap().ino);
    }
    let file = v.create(ROOT, b"file", 0o100644, 1000, 1000, Kind::Reg).unwrap().ino;
    v.dir_stat_slow(d).unwrap();
    assert_eq!(v.dirx_check(d), Ok(Some(300)));
    Fx { s, dev, v, d, file, model }
}

impl Fx {
    /// After a failed change: the volume is writable, nothing in memory is ahead of the disk (the index is gone
    /// or right, fstat's numbers are gone or right), and the folder is the model.
    fn still_right(&self, what: &str) {
        assert!(self.v.writable(), "{what}: the volume stopped");
        match self.v.dirx_check(self.d) {
            Ok(None) => {}
            Ok(Some(n)) => assert_eq!(n, self.model.len(), "{what}"),
            Err(e) => panic!("{what}: the index is ahead of the disk: {e}"),
        }
        let di = self.v.stat(self.d).unwrap();
        assert!(self.v.dir_stats().get(self.d).is_none_or(|n| n == (di.size(), di.blocks512(&self.v.geo))), "{what}: fstat's numbers");
        assert!(listing(&self.v, self.d) == self.model, "{what}: the folder is not what the model says");
    }
    /// A change that works builds the index again; then the disk: a fresh mount lists the model, e2fsck is clean.
    fn ends_clean(mut self, what: &str) {
        link(&mut self.v.lock(), self.d, b"after", self.file).unwrap();
        self.model.insert(b"after".to_vec(), self.file);
        assert_eq!(self.v.dirx_check(self.d), Ok(Some(self.model.len())), "{what}");
        assert_eq!(self.v.dirx_stales(self.d), (0, false), "{what}");
        assert!(listing(&self.v, self.d) == self.model, "{what}");
        self.v.sync_fs().unwrap();
        let img = self.dev.image();
        assert_clean(&self.s, &img, what);
        let (_d2, v2) = mount(img);
        assert!(listing(&v2, self.d) == self.model, "{what}: a fresh mount lists another folder");
    }
    /// The log counts as just too small for two of `link`'s transactions: the second one's join commits the
    /// first in the middle of the operation (`journal.full_midop`).
    fn small_log(&mut self) {
        self.v.sync_fs().unwrap();
        let mut st = self.v.lock();
        link(&mut st, self.d, b"first", self.file).unwrap();
        self.model.insert(b"first".to_vec(), self.file);
        let j = st.jnl.as_mut().unwrap();
        let own = j.run.blocks.len() as u32;
        assert!(own >= 2, "a link's transaction has {own} blocks");
        j.test_cap = own + 3;
    }
}

/// `State::commit`, the branch for a device that timed out, reached by the commit's own first write: after a
/// sync the superblock is marked "not clean" before the transaction joins.
#[test]
fn a_timeout_under_the_first_commit_after_a_sync_leaves_no_index_ahead() {
    let f = fixture();
    f.v.sync_fs().unwrap();
    assert!(!f.v.lock().dirty);
    f.dev.time_out(true);
    assert_eq!(link(&mut f.v.lock(), f.d, b"lost", f.file), Err(EIO));
    // The same through the front door: refused before its transaction begins.
    assert_eq!(f.v.create(f.d, b"lost-too", 0o100644, 1000, 1000, Kind::Reg).err(), Some(EIO));
    f.dev.time_out(false);
    f.still_right("a timeout under the first commit after a sync");
    f.ends_clean("after a timeout under the first commit after a sync");
}

/// The same branch reached by a full log: the commit in the middle of the operation times out.
#[test]
fn a_timeout_in_a_commit_in_the_middle_of_an_operation_leaves_no_index_ahead() {
    let mut f = fixture();
    f.small_log();
    f.dev.time_out(true);
    assert_eq!(link(&mut f.v.lock(), f.d, b"lost", f.file), Err(EIO));
    assert!(f.v.lock().jnl.as_ref().unwrap().retry, "no commit failed: the log was not full");
    f.dev.time_out(false);
    f.still_right("a timed-out commit in the middle of an operation");
    f.v.lock().jnl.as_mut().unwrap().test_cap = u32::MAX;
    f.ends_clean("after a timed-out commit in the middle of an operation");
}

/// Finding 6a: the same commit finds no memory for the log writer. The operation answers ENOMEM, the volume
/// stays writable (it went read-only until the next mount), and the next change retries the commit.
#[test]
fn no_memory_for_a_commit_in_the_middle_of_an_operation_keeps_the_volume_writable() {
    let mut f = fixture();
    f.small_log();
    f.v.lock().jnl.as_ref().unwrap().test_log_nomem.store(1, Ordering::Release);
    assert_eq!(link(&mut f.v.lock(), f.d, b"lost", f.file), Err(ENOMEM));
    assert!(f.v.lock().jnl.as_ref().unwrap().retry, "no commit failed: the log was not full");
    f.still_right("no memory for a commit in the middle of an operation");
    // The log is still as small: this one's join commits the kept transaction first, then joins.
    assert_eq!(f.v.create(f.d, b"next", 0o100644, 1000, 1000, Kind::Reg).map(|d| f.model.insert(b"next".to_vec(), d.ino)).err(), None);
    assert!(!f.v.lock().jnl.as_ref().unwrap().retry);
    f.v.lock().jnl.as_mut().unwrap().test_cap = u32::MAX;
    f.ends_clean("after no memory for a commit in the middle of an operation");
}

/// `State::commit`, the branch for a change that reaches a frozen volume.
#[test]
fn a_change_refused_by_a_frozen_volume_leaves_no_index_ahead() {
    let f = fixture();
    {
        let mut st = f.v.lock();
        st.frozen = true;
        let r = link(&mut st, f.d, b"lost", f.file);
        st.frozen = false;
        assert_eq!(r, Err(EBUSY));
    }
    f.still_right("a change refused by a frozen volume");
    f.ends_clean("after a change refused by a frozen volume");
}

/// The guard itself: whoever makes a `Tx`, changes a directory through it and lets it go without `State::commit`
/// or `State::abort` (an early return, a new error path) is told by every test that takes that path.
#[test]
#[should_panic(expected = "dropped without commit or abort")]
fn a_transaction_that_changed_a_directory_and_is_just_dropped_is_caught() {
    let mut tx = Tx::new();
    tx.touch_dir(ROOT);
    drop(tx);
}

#[test]
fn a_transaction_whose_directories_were_settled_goes_quietly() {
    let mut tx = Tx::new();
    tx.touch_dir(ROOT);
    tx.touch_dir(ROOT);
    assert_eq!(tx.settle_dirs(), [ROOT]);
    drop(tx);
    drop(Tx::new());
}
