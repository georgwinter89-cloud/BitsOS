//! Power cuts: every write a scenario makes is recorded; the image is rebuilt with only the first k writes for
//! every k (a cut after write k) and checked by `fsck.fat -n`. Allowed are only the findings a cut is expected
//! to leave by the write order (V1 rule: "a cut leaves at most lost clusters"):
//! - clusters no entry reaches ("Reclaimed ... unused clusters");
//! - FAT 2 behind FAT 1 ("FATs differ but appear to be intact. Using first FAT");
//! - a chain longer than its entry's size ("cluster chain length is > ..."; the size is the committed one);
//! - FSInfo's free count unknown ("uninitialized");
//! - ".." of a directory moved to another parent still naming the old one (a move changes two sectors that
//!   must agree; fsck.fat fixes the pointer, nothing is lost);
//! - the first LFN parts of a name over 195 characters without the rest (its entry set needs two sectors;
//!   fsck.fat deletes the orphaned parts by itself). Shorter names always fit one sector: one write.
//! Never: a size larger than the chain, shared clusters, broken long names, bad "." / "..", a wrong free count.
//! After `fsck.fat -a` every file that was committed before the scenario reads back unchanged.

use super::basic::sync_tree;
use super::util::*;
use std::path::Path;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};
use vfs::abi::O_RDWR;
use vfs::ops::Ctx;

fn allowed(line: &str) -> bool {
    let l = line.trim();
    l.is_empty()
        || l.starts_with("fsck.fat ")
        || l.starts_with("Leaving filesystem unchanged")
        || l.contains(" files, ")
        || l.starts_with("Free cluster summary uninitialized")
        || l.starts_with("Reclaimed ")
        || l.starts_with("FATs differ but appear to be intact")
        || l.starts_with("Using first FAT")
        || (l.starts_with("File size is ") && l.contains("cluster chain length is > "))
        || l.starts_with("Truncating file to ")
        || l.starts_with("Invalid '..' entry in the second slot. Fixing.")
        || l.starts_with("Orphaned long file name part ")
        || l.starts_with("Auto-deleting.")
        || l.starts_with('/')
}

fn check_cut(dir: &Path, base: &[u8], log: &[(u64, Vec<u8>)], keep: &[(&str, Vec<u8>)], what: &str) {
    let mut img = base.to_vec();
    for k in 0..=log.len() {
        if k > 0 {
            let (s, d) = &log[k - 1];
            let at = *s as usize * 512;
            img[at..at + d.len()].copy_from_slice(d);
        }
        let (code, out) = fsck(dir, &img);
        let bad: Vec<&str> = out.lines().filter(|l| !allowed(l)).collect();
        assert!(bad.is_empty(), "{what}: cut after write {k} of {}: fsck.fat says\n{out}", log.len());
        // A path line must be followed by the allowed size line (paths alone carry other findings too).
        let lines: Vec<&str> = out.lines().map(|l| l.trim()).collect();
        for (i, l) in lines.iter().enumerate() {
            if l.starts_with('/') && !l.contains(" files, ") {
                assert!(
                    lines.get(i + 1).is_some_and(|n| n.contains("cluster chain length is > ") || n.starts_with("Invalid '..' entry")),
                    "{what}: cut after {k}: {out}"
                );
            }
        }
        if code != 0 && (k % 7 == 0 || k == log.len()) || k == log.len() {
            let fixed = repair(dir, &img);
            assert_clean(dir, &fixed);
            let p = dir.join("fixed.img");
            std::fs::write(&p, &fixed).unwrap();
            for (path, data) in keep {
                assert!(get(&p, path).as_deref() == Some(&data[..]), "{what}: cut after {k}: committed {path} damaged");
            }
        }
    }
}

/// Runs `scenario` on a base image with committed files, records its writes and checks every cut.
fn run(tag: &str, scenario: impl FnOnce(&vfs::MountNs, &Ctx<'_>)) {
    let dir = scratch(tag);
    let img = mkfs(&dir, 34, 1, "BITSOS");
    for d in ["/keep", "/work", "/work/sub"] {
        mkdir(&img, d);
    }
    let keep = vec![("/keep/a.txt", pattern(3000, 1)), ("/keep/big.bin", pattern(100_000, 2)), ("/work/sub/file", pattern(700, 3))];
    for (p, d) in &keep {
        put_bytes(&img, &dir, p, d);
    }
    put_bytes(&img, &dir, "/work/target.txt", &pattern(9000, 4));
    let dev = load(&img);
    let base = dev.image();
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    dev.record();
    {
        let c = Ctx::kernel(&ns);
        scenario(&ns, &c);
    }
    sync_tree(&ns, "/boot");
    vol.sync(true).unwrap();
    let log = dev.take_log();
    assert!(!log.is_empty());
    // The keep files the scenario does not touch stay as they were.
    let keep: Vec<(&str, Vec<u8>)> = keep.into_iter().filter(|(p, _)| *p != "/work/sub/file").collect();
    check_cut(&dir, &base, &log, &keep, tag);
}

#[test]
fn cut_while_writing_files() {
    run("cut-files", |ns, c| {
        ns.create_file_bytes(b"/boot/work/new.bin", 0o644, &pattern(50_000, 5)).unwrap();
        sync_tree(ns, "/boot/work");
        let f = c.open(None, b"/boot/work/target.txt", O_RDWR, 0).unwrap();
        f.pwrite(9000, &pattern(10_000, 6)).unwrap();
        f.pwrite(100, b"overwritten in place").unwrap();
        f.inode().unwrap().sync(vfs::pagecache::SyncKind::Fsync).unwrap();
        drop(f);
        c.truncate(None, b"/boot/work/new.bin", 1000).unwrap();
        c.truncate(None, b"/boot/work/new.bin", 30_000).unwrap();
    });
}

#[test]
fn cut_while_changing_names() {
    run("cut-names", |ns, c| {
        ns.create_file_bytes(b"/boot/work/new.bin", 0o644, &pattern(20_000, 7)).unwrap();
        sync_tree(ns, "/boot/work");
        c.rename(None, b"/boot/work/new.bin", None, b"/boot/work/target.txt", 0).unwrap();
        c.unlink(None, b"/boot/work/sub/file").unwrap();
        c.mkdir(None, b"/boot/work/d", 0o755).unwrap();
        ns.create_file_bytes(b"/boot/work/d/inside", 0o644, b"inside").unwrap();
        sync_tree(ns, "/boot/work");
        c.rename(None, b"/boot/work/d", None, b"/boot/work/sub/d", 0).unwrap();
        c.mkdir(None, b"/boot/work/e", 0o755).unwrap();
        c.rmdir(None, b"/boot/work/e").unwrap();
        let long = format!("/boot/work/{}.txt", "L".repeat(220));
        ns.create_file_bytes(long.as_bytes(), 0o644, b"long").unwrap();
        c.rename(None, long.as_bytes(), None, b"/boot/work/short", 0).unwrap();
    });
}

#[test]
fn cut_while_a_directory_grows() {
    run("cut-grow", |ns, c| {
        c.mkdir(None, b"/boot/work/many", 0o755).unwrap();
        for i in 0..40 {
            ns.create_file_bytes(format!("/boot/work/many/file with a long name {i}").as_bytes(), 0o644, &pattern(600, i)).unwrap();
        }
        sync_tree(ns, "/boot/work/many");
        for i in (0..40).step_by(2) {
            c.unlink(None, format!("/boot/work/many/file with a long name {i}").as_bytes()).unwrap();
        }
        let _ = String::new();
    });
}
