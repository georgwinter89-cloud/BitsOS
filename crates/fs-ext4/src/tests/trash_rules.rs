//! EINS-C (K6): a restore goes only where the caller could make the name itself; the notifications of a
//! diverted removal are Linux's; a record that gets shorter never shrinks the file. e2fsck-clean after each.

use super::trash::{as_proc, entries, files, sync, tree_of, TREE};
use super::util::*;
use super::vfs::fresh_ns;
use std::format;
use std::string::String;
use vfs::inode::Cred;
use vfs::inotify::{IN_DELETE, IN_MOVED_FROM};
use vfs::ops::Ctx;
use vfs::trash::Caller;
use uapi::errno::*;

const GEORG: Caller = Caller { pid: 7400, tree: 0, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };

/// A restore moves with the kernel's rights, but not into a folder the caller may not write, nor into the trash;
/// a forged entry (the trash is the desktop user's) gets no further than its maker could go.
#[test]
fn a_restore_goes_only_where_the_caller_may_make_the_name() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("a", b"1")]);
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/data/locked", 0o755).unwrap();
    as_proc(&ns, 7401, 1000, TREE).unlink(None, b"/data/home/1000/a").unwrap();
    let id = entries(&ns).remove(0);
    let held = tree_of(&ns, &format!("/data/.papierkorb/{id}"));
    let to = |p: &[u8]| vfs::trash::restore(&ns, &GEORG, b"/data", id.as_bytes(), Some(p)).map(|r| r.moved);
    assert_eq!(to(b"/data/locked/a"), Err(EACCES), "root's folder");
    assert_eq!(to(format!("/data/.papierkorb/{id}/b").as_bytes()), Err(EINVAL), "into the trash");
    assert_eq!(to(b"/data/.papierkorb/b"), Err(EINVAL), "into the trash's top");
    assert_eq!(tree_of(&ns, &format!("/data/.papierkorb/{id}")), held, "nothing moved");
    assert_eq!(to(b"/data/home/1000/b"), Ok(1));
    assert_eq!(ns.read_file("/data/home/1000/b", 10).unwrap(), b"1");
    // A forged entry: files/locked/forged, made by uid 1000 in its own trash.
    ns.mkdir_p("/data/.papierkorb/1-1/files/locked").unwrap();
    ns.create_file_bytes(b"/data/.papierkorb/1-1/files/locked/forged", 0o644, b"f").unwrap();
    for p in ["/data/.papierkorb/1-1", "/data/.papierkorb/1-1/files", "/data/.papierkorb/1-1/files/locked", "/data/.papierkorb/1-1/files/locked/forged"] {
        super::trash::chown(&ns, p);
    }
    assert_eq!(vfs::trash::restore(&ns, &GEORG, b"/data", b"1-1", None), Err(EACCES));
    assert!(ns.lookup("/data/locked/forged").is_err(), "nothing went into root's folder");
    let root = Caller { cred: Cred::ROOT, ..GEORG };
    assert_eq!(vfs::trash::restore(&ns, &root, b"/data", b"1-1", None).map(|r| r.moved), Ok(1), "root may");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "restores refused and done");
}

/// A watcher of the folder sees a diverted unlink or rmdir as a deletion (IN_DELETE) and a diverted rename over a
/// name as Linux's one move, not as moves into the trash.
#[test]
fn a_diverted_removal_notifies_as_linux_does() {
    let s = Scratch::new();
    let (_dev, ns, _vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("w/a", b"1"), ("w/d/", b""), ("w/conf", b"old"), ("w/conf.new", b"new")]);
    let dir = ns.lookup("/data/home/1000/w").unwrap().inode.clone();
    let del = vfs::inotify::sys::new_group_for_kernel(0).unwrap();
    let mv = vfs::inotify::sys::new_group_for_kernel(0).unwrap();
    del.update_watch(&dir, IN_DELETE).unwrap();
    mv.update_watch(&dir, IN_MOVED_FROM).unwrap();
    let t = as_proc(&ns, 7410, 1000, TREE);
    t.unlink(None, b"/data/home/1000/w/a").unwrap();
    t.rmdir(None, b"/data/home/1000/w/d").unwrap();
    assert_eq!((del.queued(), mv.queued()), (2, 0), "an unlink and an rmdir: deletions");
    t.rename(None, b"/data/home/1000/w/conf.new", None, b"/data/home/1000/w/conf", 0).unwrap();
    assert_eq!((del.queued(), mv.queued()), (2, 1), "the rename: one move");
    assert_eq!(ns.read_file("/data/home/1000/w/conf", 10).unwrap(), b"new");
    let id = entries(&ns).remove(0);
    assert_eq!(ns.read_file(&format!("/data/.papierkorb/{id}/files/home/1000/w/conf"), 10).unwrap(), b"old");
    assert!(ns.lookup("/data/home/1000/w/conf.new").is_err());
}

/// The record is rewritten in place: a shorter text is padded with blanks, so the file never shrinks (a stop in
/// between leaves a whole record) and stays one JSON object.
#[test]
fn a_shorter_record_keeps_its_length() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("deep/er/x", b"1"), ("y", b"2")]);
    let t = as_proc(&ns, 7420, 1000, TREE);
    t.unlink(None, b"/data/home/1000/deep/er/x").unwrap();
    let id = entries(&ns).remove(0);
    let path = format!("/data/.papierkorb/{id}/record.json");
    let first = ns.read_file(&path, 4096).unwrap();
    t.unlink(None, b"/data/home/1000/y").unwrap();
    let now = ns.read_file(&path, 4096).unwrap();
    assert_eq!(now.len(), first.len(), "{}", String::from_utf8_lossy(&now));
    let text = std::str::from_utf8(&now).unwrap();
    assert!(text.ends_with(" \n") && text.trim_end().ends_with('}'), "{text:?}");
    assert!(text.contains("\"path\":\"/data/home/1000\""), "{text}");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "a shorter record");
}

/// A folder renamed over an empty one in another folder: the names are swapped first, then the empty one goes
/// to the trash; the link counts and `..` entries stay right (e2fsck).
#[test]
fn a_folder_over_an_empty_one_swaps_then_keeps_it() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("m/src/f", b"1"), ("n/dst/", b"")]);
    let t = as_proc(&ns, 7430, 1000, TREE);
    t.rename(None, b"/data/home/1000/m/src", None, b"/data/home/1000/n/dst", 0).unwrap();
    assert_eq!(ns.read_file("/data/home/1000/n/dst/f", 10).unwrap(), b"1");
    assert!(ns.lookup("/data/home/1000/m/src").is_err());
    let id = entries(&ns).remove(0);
    assert!(ns.lookup(&format!("/data/.papierkorb/{id}/files/home/1000/n/dst")).unwrap().is_dir());
    let nlink = |p: &str| t.stat(None, p.as_bytes(), false).unwrap().nlink;
    assert_eq!((nlink("/data/home/1000/m"), nlink("/data/home/1000/n")), (2, 3));
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "a folder over an empty one");
}
