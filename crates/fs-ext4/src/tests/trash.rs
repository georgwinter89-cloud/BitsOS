//! EINS-C (K6): the assistant tree's trash on ext4 through the VFS: what goes to `/data/.papierkorb`, what is
//! removed for real, one entry per command, the record, the full disk; each sequence e2fsck-clean.

use super::util::*;
use super::vfs::fresh_ns;
use std::string::String;
use std::vec::Vec;
use std::{format, vec};
use vfs::abi::{O_CREAT, O_RDWR, O_WRONLY, RENAME_EXCHANGE, RENAME_NOREPLACE};
use vfs::inode::Cred;
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

pub(super) const TREE: u32 = 41;

/// A process of tree `tree` (0: none) as `uid`.
pub(super) fn as_proc(ns: &vfs::MountNs, pid: u32, uid: u32, tree: u32) -> Ctx<'_> {
    let mut c = Ctx::kernel(ns);
    c.cred = Cred { uid, gid: uid };
    c.umask = 0o022;
    c.subject = policy::Subject { program: policy::ProgramId(1), pid, uid, tree };
    c
}

/// Georg's files: `/data/home/1000/<rel>` = contents, made by the kernel and given to uid 1000.
pub(super) fn files(ns: &vfs::MountNs, list: &[(&str, &[u8])]) {
    let k = Ctx::kernel(ns);
    ns.mkdir_p("/data/home/1000").unwrap();
    chown(ns, "/data/home");
    chown(ns, "/data/home/1000");
    for (rel, data) in list {
        let comps: Vec<&str> = rel.trim_end_matches('/').split('/').collect();
        let dirs = if rel.ends_with('/') { comps.len() } else { comps.len() - 1 };
        let mut p = String::from("/data/home/1000");
        for c in &comps[..dirs] {
            p.push('/');
            p.push_str(c);
            if k.mkdir(None, p.as_bytes(), 0o755).is_ok() {
                chown(ns, &p);
            }
        }
        if !rel.ends_with('/') {
            let path = format!("/data/home/1000/{rel}");
            ns.create_file_bytes(path.as_bytes(), 0o644, data).unwrap();
            chown(ns, &path);
        }
    }
}

pub(super) fn chown(ns: &vfs::MountNs, path: &str) {
    let p = ns.lookup_path(path.trim_end_matches('/').as_bytes(), false).unwrap();
    Ctx::kernel(ns).setattr(&p, |m| vfs::setattr::chown(m, Cred::ROOT, 1000, 1000)).unwrap();
}

/// Every name below `path` with its type, mode, owner and contents (sorted).
pub(super) fn tree_of(ns: &vfs::MountNs, path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut work = vec![String::from(path)];
    while let Some(d) = work.pop() {
        for (n, a) in ns.list_dir(&d).unwrap() {
            let p = format!("{d}/{}", String::from_utf8(n).unwrap());
            let kind = a.mode & uapi::S_IFMT;
            let body = if kind == uapi::S_IFREG { format!("{:?}", ns.read_file(&p, 1 << 20).unwrap()) } else { String::new() };
            out.push(format!("{} {:o} {}:{} {body}", &p[path.len()..], a.mode, a.uid, a.gid));
            if kind == uapi::S_IFDIR {
                work.push(p);
            }
        }
    }
    out.sort();
    out
}

/// The entries of `/data/.papierkorb` (empty when it is not there).
pub(super) fn entries(ns: &vfs::MountNs) -> Vec<String> {
    let mut v: Vec<String> = ns.list_dir("/data/.papierkorb").map(|l| l.into_iter().map(|(n, _)| String::from_utf8(n).unwrap()).collect()).unwrap_or_default();
    v.sort();
    v
}

pub(super) fn record(ns: &vfs::MountNs, id: &str) -> String {
    String::from_utf8(ns.read_file(&format!("/data/.papierkorb/{id}/record.json"), 8192).unwrap()).unwrap()
}

pub(super) fn sync(ns: &vfs::MountNs, vol: &crate::Volume, path: &str) {
    let mut work = vec![String::from(path)];
    while let Some(d) = work.pop() {
        for (n, a) in ns.list_dir(&d).unwrap() {
            let p = format!("{d}/{}", String::from_utf8(n).unwrap());
            match a.mode & uapi::S_IFMT {
                uapi::S_IFDIR => work.push(p),
                uapi::S_IFREG => ns.lookup(&p).unwrap().inode.sync(SyncKind::Fsync).unwrap(),
                _ => {}
            }
        }
    }
    vol.sync_fs().unwrap();
}

#[test]
fn an_unlink_of_the_tree_goes_to_the_trash() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("brief.txt", b"Lieber Georg")]);
    let t = as_proc(&ns, 7001, 1000, TREE);
    t.unlink(None, b"/data/home/1000/brief.txt").unwrap();
    assert_eq!(t.stat(None, b"/data/home/1000/brief.txt", false).err(), Some(uapi::errno::ENOENT));
    let e = entries(&ns);
    assert_eq!(e.len(), 1, "{e:?}");
    let id = &e[0];
    assert_eq!(ns.read_file(&format!("/data/.papierkorb/{id}/files/home/1000/brief.txt"), 100).unwrap(), b"Lieber Georg");
    let r = record(&ns, id);
    assert!(r.contains("\"path\":\"/data/home/1000/brief.txt\"") && r.contains("\"what\":\"unlink\"") && r.contains("\"pid\":7001") && r.contains(&format!("\"tree\":{TREE}")), "{r}");
    // the trash is Georg's, kept to him
    let a = t.stat(None, b"/data/.papierkorb", false).unwrap();
    assert_eq!((a.uid, a.mode & 0o7777), (1000, 0o700));
    // the copied parents keep the originals' owners and modes
    let h = t.stat(None, format!("/data/.papierkorb/{id}/files/home/1000").as_bytes(), false).unwrap();
    assert_eq!((h.uid, h.mode & 0o7777), (1000, 0o755));
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "an unlink into the trash");
}

#[test]
fn outside_the_tree_a_removal_is_real() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("a", b"1"), ("d/b", b"2"), ("old", b"3"), ("new", b"4")]);
    let g = as_proc(&ns, 7002, 1000, 0);
    g.unlink(None, b"/data/home/1000/a").unwrap();
    g.unlink(None, b"/data/home/1000/d/b").unwrap();
    g.rmdir(None, b"/data/home/1000/d").unwrap();
    g.rename(None, b"/data/home/1000/new", None, b"/data/home/1000/old", 0).unwrap();
    assert!(entries(&ns).is_empty());
    assert_eq!(ns.read_file("/data/home/1000/old", 10).unwrap(), b"4");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "real removals outside the tree");
}

/// `rm -r` as busybox does it (depth first): one entry, the record names the top, everything restorable.
#[test]
fn rm_r_is_one_entry_and_comes_back_whole() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("proj/a.txt", b"a"), ("proj/sub/b.txt", b"b"), ("proj/sub/deep/c.txt", b"c"), ("proj/empty/", b"")]);
    let before = tree_of(&ns, "/data/home/1000");
    let t = as_proc(&ns, 7003, 1000, TREE);
    for f in ["proj/sub/deep/c.txt", "proj/sub/b.txt", "proj/a.txt"] {
        let p = format!("/data/home/1000/{f}");
        t.unlink(None, p.as_bytes()).unwrap();
        if f == "proj/sub/deep/c.txt" {
            t.rmdir(None, b"/data/home/1000/proj/sub/deep").unwrap();
        }
        if f == "proj/sub/b.txt" {
            t.rmdir(None, b"/data/home/1000/proj/sub").unwrap();
            t.rmdir(None, b"/data/home/1000/proj/empty").unwrap();
        }
    }
    t.rmdir(None, b"/data/home/1000/proj").unwrap();
    assert!(ns.list_dir("/data/home/1000").unwrap().is_empty());
    let e = entries(&ns);
    assert_eq!(e.len(), 1, "{e:?}");
    let r = record(&ns, &e[0]);
    assert!(r.contains("\"path\":\"/data/home/1000/proj\""), "{r}");
    let held = tree_of(&ns, &format!("/data/.papierkorb/{}/files/home/1000", e[0]));
    assert_eq!(held, before, "the entry holds the tree as it was");
    let who = vfs::trash::Caller { pid: 7004, tree: 0, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };
    let back = vfs::trash::restore(&ns, &who, b"/data", e[0].as_bytes(), None).unwrap();
    assert_eq!(back.moved, 1, "proj went back whole");
    assert_eq!(tree_of(&ns, "/data/home/1000"), before);
    assert!(entries(&ns).is_empty(), "the restored entry is gone");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "rm -r into the trash and back");
}

/// A pause, another process or another command: separate entries.
#[test]
fn a_new_process_or_a_pause_starts_a_new_entry() {
    let s = Scratch::new();
    let (_dev, ns, _vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("a", b"1"), ("b", b"2"), ("c", b"3"), ("d", b"4")]);
    let one = as_proc(&ns, 7010, 1000, TREE);
    one.unlink(None, b"/data/home/1000/a").unwrap();
    one.unlink(None, b"/data/home/1000/b").unwrap();
    assert_eq!(entries(&ns).len(), 1, "one command");
    as_proc(&ns, 7011, 1000, TREE).unlink(None, b"/data/home/1000/c").unwrap();
    assert_eq!(entries(&ns).len(), 2, "another process");
    vfs::trash::end_batches_of(7010);
    one.unlink(None, b"/data/home/1000/d").unwrap();
    assert_eq!(entries(&ns).len(), 3, "after a pause");
    let first = entries(&ns).into_iter().find(|id| ns.lookup(&format!("/data/.papierkorb/{id}/files/home/1000/a")).is_ok()).unwrap();
    assert!(ns.lookup(&format!("/data/.papierkorb/{first}/files/home/1000/b")).is_ok(), "a and b are one entry");
    let r = record(&ns, &first);
    assert!(r.contains("\"path\":\"/data/home/1000\""), "the top of a and b: {r}");
}

/// The tree's own files go for real; once Georg opened one, it is his.
#[test]
fn own_files_are_removed_for_real_until_georg_opened_them() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[]);
    let t = as_proc(&ns, 7020, 1000, TREE);
    for n in ["tmp1", "seen"] {
        let f = t.open(None, format!("/data/home/1000/{n}").as_bytes(), O_WRONLY | O_CREAT, 0o600).unwrap();
        f.pwrite(0, b"x").unwrap();
    }
    t.mkdir(None, b"/data/home/1000/tmpdir", 0o700).unwrap();
    t.unlink(None, b"/data/home/1000/tmp1").unwrap();
    t.rmdir(None, b"/data/home/1000/tmpdir").unwrap();
    assert!(entries(&ns).is_empty(), "the tree's own temp files do not fill the trash");
    drop(as_proc(&ns, 7021, 1000, 0).open(None, b"/data/home/1000/seen", O_RDWR, 0).unwrap());
    t.unlink(None, b"/data/home/1000/seen").unwrap();
    assert_eq!(entries(&ns).len(), 1, "Georg opened it: it goes to the trash");
    // another tree's file counts as Georg's
    let f = as_proc(&ns, 7022, 1000, TREE + 1).open(None, b"/data/home/1000/other", O_WRONLY | O_CREAT, 0o600).unwrap();
    drop(f);
    t.unlink(None, b"/data/home/1000/other").unwrap();
    assert_eq!(ns.list_dir(&format!("/data/.papierkorb/{}/files/home/1000", entries(&ns)[0])).unwrap().len(), 2);
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "own files");
}

/// A name of a file with another name: removed for real, the data stays under the other; the last name goes
/// to the trash.
#[test]
fn a_hard_link_keeps_the_data() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("one", b"data")]);
    Ctx::kernel(&ns).link(None, b"/data/home/1000/one", None, b"/data/home/1000/two", false).unwrap();
    let t = as_proc(&ns, 7030, 1000, TREE);
    t.unlink(None, b"/data/home/1000/one").unwrap();
    assert!(entries(&ns).is_empty());
    t.unlink(None, b"/data/home/1000/two").unwrap();
    let e = entries(&ns);
    assert_eq!(ns.read_file(&format!("/data/.papierkorb/{}/files/home/1000/two", e[0]), 10).unwrap(), b"data");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "hard links");
}

/// rename over a name: the old file goes to the trash, in one directory and across; EXCHANGE and NOREPLACE
/// lose nothing and keep nothing.
#[test]
fn a_rename_over_a_file_keeps_the_old_one() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("conf", b"old"), ("conf.new", b"new"), ("x/far", b"far"), ("y/near", b"near"), ("p", b"p"), ("q", b"q")]);
    let t = as_proc(&ns, 7040, 1000, TREE);
    t.rename(None, b"/data/home/1000/conf.new", None, b"/data/home/1000/conf", 0).unwrap();
    assert_eq!(ns.read_file("/data/home/1000/conf", 10).unwrap(), b"new");
    t.rename(None, b"/data/home/1000/x/far", None, b"/data/home/1000/y/near", 0).unwrap();
    assert_eq!(ns.read_file("/data/home/1000/y/near", 10).unwrap(), b"far");
    t.rename(None, b"/data/home/1000/p", None, b"/data/home/1000/q", RENAME_EXCHANGE).unwrap();
    assert_eq!(t.rename(None, b"/data/home/1000/p", None, b"/data/home/1000/q", RENAME_NOREPLACE).err(), Some(uapi::errno::EEXIST));
    let e = entries(&ns);
    assert_eq!(e.len(), 1, "{e:?}");
    let base = format!("/data/.papierkorb/{}/files/home/1000", e[0]);
    assert_eq!(ns.read_file(&format!("{base}/conf"), 10).unwrap(), b"old");
    assert_eq!(ns.read_file(&format!("{base}/y/near"), 10).unwrap(), b"near");
    assert!(record(&ns, &e[0]).contains("\"what\":\"rename\""));
    // a tool writing its own file anew (write a temp file, rename it over): nothing is kept of its own drafts
    vfs::trash::end_batches_of(7040);
    for round in 0..3 {
        let f = t.open(None, b"/data/home/1000/draft.tmp", O_WRONLY | O_CREAT, 0o600).unwrap();
        f.pwrite(0, format!("{round}").as_bytes()).unwrap();
        drop(f);
        t.rename(None, b"/data/home/1000/draft.tmp", None, b"/data/home/1000/draft", 0).unwrap();
    }
    assert_eq!(entries(&ns).len(), 1, "its own drafts were replaced for real");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "renames over names");
}

/// tmpfs is not a persistent file system: the tree removes there for real.
#[test]
fn tmpfs_is_exempt() {
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/tmp/w").unwrap();
    ns.create_file_bytes(b"/tmp/w/f", 0o644, b"x").unwrap();
    let t = as_proc(&ns, 7050, 0, TREE);
    t.unlink(None, b"/tmp/w/f").unwrap();
    t.rmdir(None, b"/tmp/w").unwrap();
    assert!(ns.list_dir("/tmp/.papierkorb").is_err() && ns.list_dir("/.papierkorb").is_err());
}

/// A nearly full file system: the tree's removal is refused (ENOSPC), nothing is destroyed; outside the tree the
/// removal frees the room.
#[test]
fn a_full_disk_refuses_instead_of_destroying() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("keep", b"precious")]);
    let k = Ctx::kernel(&ns);
    let f = k.open(None, b"/data/fill", O_WRONLY | O_CREAT, 0o600).unwrap();
    let block = vec![0x5Au8; 256 * 1024];
    let mut at = 0u64;
    while f.pwrite(at, &block).is_ok_and(|n| n == block.len()) {
        at += block.len() as u64;
    }
    drop(f);
    let t = as_proc(&ns, 7060, 1000, TREE);
    assert_eq!(t.unlink(None, b"/data/home/1000/keep").err(), Some(uapi::errno::ENOSPC));
    assert_eq!(ns.read_file("/data/home/1000/keep", 100).unwrap(), b"precious");
    k.unlink(None, b"/data/fill").unwrap();
    t.unlink(None, b"/data/home/1000/keep").unwrap();
    assert_eq!(entries(&ns).len(), 1);
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "a full disk");
}

/// Every removing call through its syscall entry point (the ktest `fs.ext4.trash_calls`'s body on the host).
#[test]
fn sys_variants() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    let ns = std::sync::Arc::new(ns);
    crate::ktests_trash::sys_variants(&ns, TREE + 2).unwrap();
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "every removing call");
}
