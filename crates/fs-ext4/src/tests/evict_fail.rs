//! Finding 6b of the BIGDIR hardening: an unlink or a rename over a file whose name change is committed answers
//! Ok even when freeing the inode right after fails (here: a device read error at each read of the freeing in
//! turn). The answer and the disk never disagree: an error is given only while the name is still there. An inode
//! not freed stays on the orphan list when the failure came before the transaction that takes it off, and the
//! next mount frees it; a failure after that transaction (the inode is deleted, its blocks and its bit are not
//! yet given back) leaves what e2fsck calls bitmap differences, as a power cut at that place always did. With a
//! journal (FSR) there is no such place: the frees are in the transaction that takes the inode off the list.

use super::bigdir_dev::mount_counted;
use super::util::*;
use super::write::{file, put};
use crate::inode::ROOT;
use crate::le::g32;
use crate::sb;
use crate::Volume;
use std::format;

fn has(v: &Volume, name: &[u8]) -> bool {
    v.list(ROOT).unwrap().iter().any(|e| e.0 == name)
}

fn on_orphan_list(v: &Volume, ino: u32) -> bool {
    let st = v.lock();
    let mut cur = g32(&st.sb, sb::LAST_ORPHAN);
    for _ in 0..1000 {
        if cur == 0 || cur == ino {
            break;
        }
        cur = st.inode(None, cur).unwrap().dtime();
    }
    cur == ino
}

fn free(v: &Volume) -> (u64, u32) {
    let st = v.lock();
    (st.free_now(), st.free_inodes())
}

#[test]
fn a_failed_free_after_the_committed_unlink_is_not_the_unlinks_answer() {
    run(&["-b", "4096", "-N", "1024", "-O", "^has_journal"], false);
}

/// FSR: with a journal the frees are in the transaction that deletes the inode and takes it off the orphan list,
/// so a failure anywhere in the freeing leaves the whole inode to the next mount; nothing is left to e2fsck.
#[test]
fn with_a_journal_a_failed_free_leaves_the_whole_inode_to_the_next_mount() {
    run(&["-b", "4096", "-N", "1024"], true);
}

fn run(layout: &[&str], journal: bool) {
    for over in [false, true] {
        let s = Scratch::new();
        let (dev0, v0) = mount_counted(mkfs(&s, "16M", layout, None));
        let victim = file(&v0, ROOT, "victim");
        put(&v0, victim, 0, &[7u8; 40_000]);
        let mover = file(&v0, ROOT, "mover");
        v0.sync_fs().unwrap();
        let base = dev0.inner.image();
        drop(v0);
        // What the volume has free once the victim is gone.
        let want = {
            let (_d, v) = mount_counted(base.clone());
            v.remove(ROOT, b"victim", victim).unwrap();
            if over {
                v.remove(ROOT, b"mover", mover).unwrap();
                file(&v, ROOT, "victim");
            }
            free(&v)
        };
        let (mut refused, mut by_next_mount, mut left_to_fsck, mut stopped) = (0, 0, 0, 0);
        for n in 1..200i64 {
            let what = format!("{} with read {n} failing", if over { "rename over" } else { "unlink" });
            let (dev, v) = mount_counted(base.clone());
            dev.fail_read(n);
            let r = if over { v.rename(ROOT, b"mover", mover, ROOT, b"victim", Some(victim)) } else { v.remove(ROOT, b"victim", victim) };
            let unused = dev.fail_read_left() > 0;
            dev.fail_read(0);
            let gone = if over { !has(&v, b"mover") } else { !has(&v, b"victim") };
            assert_eq!(r.is_ok(), gone, "{what}: answered {r:?}, the name change is {}", if gone { "on the disk" } else { "not done" });
            if journal && !v.writable() {
                // A read the commit itself needed (the superblock's "in use" at the first change after the mount):
                // the volume stops as Linux's journal aborts. What reached the disk must be whole.
                stopped += 1;
                drop(v);
                let (d2, v2) = mount_counted(dev.inner.image());
                v2.sync_fs().unwrap();
                assert_clean(&s, &d2.inner.image(), &format!("{what}: stopped, then the next mount"));
                continue;
            }
            assert!(v.writable(), "{what}: a failed read stopped the volume");
            if unused {
                assert_eq!(free(&v), want, "{what} (no read failed)");
                break;
            }
            if !gone {
                refused += 1;
                v.sync_fs().unwrap();
                assert_clean(&s, &dev.inner.image(), &what);
                continue;
            }
            let orphan = on_orphan_list(&v, victim);
            v.sync_fs().unwrap();
            let img = dev.inner.image();
            drop(v);
            if orphan {
                // The next mount's orphan cleanup frees it.
                by_next_mount += 1;
                let (d2, v2) = mount_counted(img);
                assert!(!on_orphan_list(&v2, victim), "{what}: still an orphan after the next mount");
                assert_eq!(free(&v2), want, "{what}: free blocks and inodes after the next mount");
                v2.sync_fs().unwrap();
                assert_clean(&s, &d2.inner.image(), &format!("{what}, after the next mount"));
            } else {
                // Deleted on the disk, blocks or bit not given back: e2fsck's, and nothing else is wrong.
                left_to_fsck += 1;
                let (fix, again, out) = repair(&s, &img);
                assert!(fix <= 1 && again == 0, "{what}: e2fsck -fy {fix}, then -fn {again}\n{out}");
            }
        }
        std::println!("evict after {} ({layout:?}): {refused} failures before the commit (error, name kept), {by_next_mount} after it freed by the next mount, {left_to_fsck} left to e2fsck, {stopped} stopped the volume", if over { "rename over" } else { "unlink" });
        assert!(refused > 0 && by_next_mount > 0, "no read failed {}", if refused == 0 { "before the commit" } else { "in the freeing" });
        assert!(!journal || left_to_fsck == 0, "{layout:?}: {left_to_fsck} failures in the freeing left an inode deleted with its blocks or its bit still taken");
    }
}
