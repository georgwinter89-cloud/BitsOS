//! FS-W2: blocks that change sides on the disk path. A directory's blocks reach the disk through the disk's cache
//! (journal, home writes); a file's data goes beside it. When a block that was the one becomes the other, neither
//! the cache (an old page of it) nor the journal (an old logged copy) may put the old content back.
//!
//! The volume is filled to the last block first, so the blocks a new file or directory gets are exactly the ones
//! just freed.
//!
//! The journal's part (nothing new in FS-W2, shown here): freeing a block that the live log holds a copy of puts a
//! revoke record into the freeing transaction and takes the block out of the running one (`State::join`: Linux's
//! `jbd2_journal_revoke` and `jbd2_journal_forget`, from `__ext4_forget`, linux-7/fs/ext4/ext4_jbd2.c:275); the
//! block stays unusable until that transaction is committed (`busy`), and commits are one at a time, so every home
//! write of the old copy happens before the block can be claimed. A replay skips the revoked copy
//! (`recovery::replay`): the last test cuts with the old copy, the revoke and the file's mapping all in the log.

use super::disk::*;
use super::flight::{pattern, read_all, step, MIB};
use super::util::*;
use super::write::file;
use crate::errno::ENOSPC;
use crate::inode::ROOT;
use crate::{Kind, Volume};
use std::collections::BTreeSet;
use std::format;
use std::string::String;
use std::vec;
use std::vec::Vec;

const LAYOUTS: [(&str, &[&str]); 2] = [("journal", &["-b", "4096", "-m", "0"]), ("bare", &["-b", "4096", "-m", "0", "-O", "^has_journal"])];

fn long(i: usize) -> String {
    format!("an-entry-with-a-name-long-enough-to-fill-directory-blocks-quickly-{i:05}-{}", "x".repeat(120))
}

/// A file over every free block: megabytes, then smaller pieces, until the volume says ENOSPC to a single block.
fn fill(v: &Volume, name: &str) -> u32 {
    let f = file(v, ROOT, name);
    let junk = vec![0x5Au8; MIB];
    let mut at = 0u64;
    for piece in [MIB, 64 << 10, 4096] {
        loop {
            match step(v, f, at, &junk[..piece], at + piece as u64) {
                Ok(()) => at += piece as u64,
                Err(ENOSPC) => break,
                Err(e) => panic!("filling the volume: errno {e}"),
            }
        }
    }
    f
}

/// Writes a file over what is free, block by block; returns its inode and bytes.
fn take_the_rest(v: &Volume, name: &str, seed: u32) -> (u32, Vec<u8>) {
    let f = file(v, ROOT, name);
    let free = v.st.lock().free_now() as usize;
    let data = pattern(free * 4096, seed);
    let mut len = 0;
    while len < data.len() && step(v, f, len as u64, &data[len..len + 4096], (len + 4096) as u64).is_ok() {
        len += 4096;
    }
    (f, data[..len].to_vec())
}

/// The file's blocks on the raw disk, in file order.
fn raw(ld: &LogDisk, v: &Volume, ino: u32) -> Vec<u8> {
    let img = ld.snapshot();
    blocks_of(v, ino).iter().flat_map(|&b| img[b as usize * 4096..(b as usize + 1) * 4096].to_vec()).collect()
}

/// A directory with `n` long names, synced; returns it, its entries and its blocks.
fn directory(v: &Volume, n: usize) -> (u32, Vec<(String, u32)>, BTreeSet<u64>) {
    let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
    let names: Vec<(String, u32)> = (0..n).map(|i| (long(i), file(v, d, &long(i)))).collect();
    v.sync_fs().unwrap();
    let blocks: BTreeSet<u64> = blocks_of(v, d).into_iter().collect();
    assert!(blocks.len() >= 4, "the directory has {} blocks", blocks.len());
    (d, names, blocks)
}

/// The directory's blocks get dirty pages in the disk's cache (a change to every block, committed: home writes),
/// then the directory goes.
fn dirty_then_remove(v: &Volume, d: u32, names: &[(String, u32)]) {
    for (name, ino) in names.iter().step_by(2) {
        v.remove(d, name.as_bytes(), *ino).unwrap();
    }
    v.flush().unwrap();
    for (name, ino) in names.iter().skip(1).step_by(2) {
        v.remove(d, name.as_bytes(), *ino).unwrap();
    }
    v.remove(ROOT, b"d", d).unwrap();
}

#[test]
fn directory_blocks_that_become_file_data_keep_the_data() {
    for (layout, opts_) in LAYOUTS {
        let s = Scratch::new();
        let (ld, _dev, v) = disk_volume(&s, "32M", opts_);
        let (d, names, dir_blocks) = directory(&v, 300);
        fill(&v, "fill");
        v.sync_fs().unwrap();
        dirty_then_remove(&v, d, &names);
        // No flush in between: without a journal the old pages are still dirty when the file takes the blocks;
        // with one the allocator's own commit has written them, and they sit in the cache clean.
        let (new, data) = take_the_rest(&v, "new", 77);
        let got: BTreeSet<u64> = blocks_of(&v, new).into_iter().collect();
        let reused = got.intersection(&dir_blocks).count();
        assert!(reused >= 3, "{layout}: the file took {reused} of the directory's {} blocks: the test shows nothing", dir_blocks.len());
        // Through the disk's cache (`Volume::read`): an old page of a block would be read instead of the data.
        assert!(read_all(&v, new) == data, "{layout}: the file reads other bytes than written, through the disk's cache");
        v.flush().unwrap();
        // On the disk: an old dirty page written by that flush would lie over the data.
        assert!(raw(&ld, &v, new) == data, "{layout}: old directory blocks were written over the file's data");
        // Later commits and the checkpoint at sync: still the data.
        for i in 0..3 {
            file(&v, ROOT, &format!("later-{i}"));
            v.flush().unwrap();
        }
        v.sync_fs().unwrap();
        assert!(raw(&ld, &v, new) == data, "{layout}: a later checkpoint put an old directory block back");
        let img = ld.snapshot();
        assert_clean(&s, &img, layout);
        let (_d2, v2) = mount(img);
        assert!(read_all(&v2, lookup(&v2, "/new")) == data, "{layout}: after a remount");
    }
}

/// The reverse: a file's blocks (written beside the cache, and read through it once, so old data pages sit in it)
/// become directory blocks. With 4 KiB blocks a metadata write replaces its whole page, so the cache cannot mix
/// the two; what is checked is that the directory is whole on the disk, after a replay too.
#[test]
fn file_blocks_that_become_directory_blocks_hold_the_directory() {
    for (layout, opts_) in LAYOUTS {
        let s = Scratch::new();
        let (ld, _dev, v) = disk_volume(&s, "32M", opts_);
        let old = file(&v, ROOT, "old");
        let data = pattern(96 * 4096, 5);
        step(&v, old, 0, &data, data.len() as u64).unwrap();
        v.flush().unwrap();
        assert!(read_all(&v, old) == data);
        let old_blocks: BTreeSet<u64> = blocks_of(&v, old).into_iter().collect();
        fill(&v, "fill");
        v.sync_fs().unwrap();
        v.remove(ROOT, b"old", old).unwrap();
        let d = v.create(ROOT, b"d", 0o40755, 1000, 1000, Kind::Dir).unwrap().ino;
        let mut names = Vec::new();
        for i in 0..4000 {
            match v.create(d, long(i).as_bytes(), 0o100644, 1000, 1000, Kind::Reg) {
                Ok(_) => names.push(long(i)),
                Err(ENOSPC) => break,
                Err(e) => panic!("{layout}: errno {e}"),
            }
        }
        let got: BTreeSet<u64> = blocks_of(&v, d).into_iter().collect();
        let reused = got.intersection(&old_blocks).count();
        assert!(reused >= 8, "{layout}: the directory took {reused} of the file's blocks: the test shows nothing");
        let listed = |v: &Volume, d: u32| -> BTreeSet<String> { v.list(d).unwrap().into_iter().map(|(n, _, _)| String::from_utf8(n).unwrap()).filter(|n| n != "." && n != "..").collect() };
        let want: BTreeSet<String> = names.iter().cloned().collect();
        assert!(listed(&v, d) == want, "{layout}: the directory lists other names");
        v.flush().unwrap();
        if layout == "journal" {
            // Cut here: the directory's blocks are in the log (and home, or not yet); the replay writes them.
            let (_d2, v2) = mount(ld.snapshot());
            assert!(listed(&v2, lookup(&v2, "/d")) == want, "{layout}: after a replay");
        }
        v.sync_fs().unwrap();
        let img = ld.snapshot();
        assert_clean(&s, &img, layout);
        let (_d3, v3) = mount(img);
        assert!(listed(&v3, lookup(&v3, "/d")) == want, "{layout}: after a remount");
    }
}

/// The cut with everything still in the log: the directory blocks' logged copies (transaction 1), their revokes
/// (the transaction that freed them) and the file's mapping. The replay must leave the file's data alone.
#[test]
fn a_replay_does_not_put_a_logged_directory_block_over_the_file_data_written_beside_the_cache() {
    let s = Scratch::new();
    let (ld, _dev, v) = disk_volume(&s, "32M", LAYOUTS[0].1);
    let (d, names, dir_blocks) = directory(&v, 300);
    fill(&v, "fill");
    // The log is empty from here on: what follows stays in it until the cut.
    v.sync_fs().unwrap();
    dirty_then_remove(&v, d, &names);
    let (new, data) = take_the_rest(&v, "new", 78);
    let reused = blocks_of(&v, new).into_iter().filter(|b| dir_blocks.contains(b)).count();
    assert!(reused >= 3, "the file took {reused} of the directory's blocks: the test shows nothing");
    v.flush().unwrap();
    let img = ld.snapshot();
    // The log holds a copy of a block the file owns now (the replay has something to skip).
    let j = debugfs(&s, &img, "logdump -a");
    let logged = dir_blocks.iter().filter(|b| j.contains(&format!("FS block {b} logged"))).count();
    let revoked = dir_blocks.iter().filter(|b| j.contains(&format!("Revoke FS block {b}"))).count();
    assert!(logged >= 1 && revoked >= 1, "the log holds {logged} copies and {revoked} revokes of the directory's blocks: the test shows nothing\n{}", j.lines().take(40).collect::<Vec<_>>().join("\n"));
    let (cut, v2) = mount(img);
    assert!(read_all(&v2, lookup(&v2, "/new")) == data, "the replay wrote an old directory block over the file's data");
    v2.sync_fs().unwrap();
    drop(v2);
    assert_clean(&s, &cut.image(), "after the replay");
}
