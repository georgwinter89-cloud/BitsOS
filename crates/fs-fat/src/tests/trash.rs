//! EINS-C (K6) on the boot stick: the assistant tree's removal below `\BITSOS2\home\` goes to
//! `\BITSOS2\home\.papierkorb` (the stick's top takes no writes; `home` is the desktop user's), comes back through
//! a restore, and a stick without `home` refuses the removal instead of destroying. fsck.fat clean after.

use super::rules::stick_image;
use super::util::*;
use std::format;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;
use uapi::errno::*;
use vfs::inode::Cred;
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

fn tree_ctx(ns: &vfs::MountNs) -> Ctx<'_> {
    let mut c = Ctx::kernel(ns);
    c.cred = Cred { uid: 1000, gid: 1000 };
    c.subject = policy::Subject { program: policy::ProgramId(1), pid: 7300, uid: 1000, tree: 61 };
    c
}

fn entries(ns: &vfs::MountNs) -> Vec<String> {
    ns.list_dir("/boot/BITSOS2/home/.papierkorb").map(|l| l.into_iter().map(|(n, _)| String::from_utf8(n).unwrap()).collect()).unwrap_or_default()
}

#[test]
fn a_tree_removal_on_the_stick_goes_to_the_home_trash() {
    let dir = scratch("trash");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/boot/BITSOS2/home", 0o700).unwrap();
    k.mkdir(None, b"/boot/BITSOS2/home/cobalt", 0o700).unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/home/cobalt/Lesezeichen", 0o600, b"marks").unwrap();
    let t = tree_ctx(&ns);
    t.unlink(None, b"/boot/BITSOS2/home/cobalt/Lesezeichen").unwrap();
    let e = entries(&ns);
    assert_eq!(e.len(), 1, "{e:?}");
    let kept = format!("/boot/BITSOS2/home/.papierkorb/{}/files/BITSOS2/home/cobalt/Lesezeichen", e[0]);
    assert_eq!(ns.read_file(&kept, 100).unwrap(), b"marks");
    let a = t.stat(None, b"/boot/BITSOS2/home/.papierkorb", false).unwrap();
    assert_eq!((a.uid, a.mode & 0o777), (1000, 0o700), "the stick's trash is the desktop user's");
    // the tree cannot remove from it
    assert_eq!(t.unlink(None, kept.as_bytes()).err(), Some(EPERM));
    let georg = vfs::trash::Caller { pid: 7301, tree: 0, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };
    assert_eq!(vfs::trash::restore(&ns, &georg, b"/boot", e[0].as_bytes(), None).map(|r| r.moved), Ok(1));
    assert_eq!(ns.read_file("/boot/BITSOS2/home/cobalt/Lesezeichen", 100).unwrap(), b"marks");
    assert!(entries(&ns).is_empty());
    ns.lookup("/boot/BITSOS2/home/cobalt/Lesezeichen").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
}

#[test]
fn without_home_the_stick_refuses_the_removal() {
    let dir = scratch("trash-nohome");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let mut t = tree_ctx(&ns);
    t.cred = Cred::ROOT;
    assert_eq!(t.unlink(None, b"/boot/BITSOS2/bin/hello").err(), Some(EROFS));
    assert!(ns.read_file("/boot/BITSOS2/bin/hello", 1 << 20).is_ok(), "the file was lost");
}

/// A tree's rename over a name whose second step the file system refuses: vfat cannot swap two names, so the
/// replaced file leaves first and comes back when the rename fails. Onto CONFIG.TXT (in the boot view the write
/// guard refuses this rename before: here the view is the test's own) R8a refuses the way back too, as it sees
/// a new CONFIG.TXT with a guarded line: then the old file stays in the trash with its record, nothing is lost.
#[test]
fn a_refused_rename_over_a_name_loses_nothing() {
    let dir = scratch("trash-undo");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/boot/BITSOS2/home", 0o700).unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/new.txt", 0o644, b"screen=1\nnvme_write=off\n").unwrap();
    let mut t = tree_ctx(&ns);
    t.cred = Cred::ROOT;
    assert_eq!(t.rename(None, b"/boot/BITSOS2/new.txt", None, b"/boot/BITSOS2/CONFIG.TXT", 0).err(), Some(EPERM));
    assert_eq!(ns.read_file("/boot/BITSOS2/new.txt", 100).unwrap(), b"screen=1\nnvme_write=off\n", "the new file stays");
    let e = entries(&ns);
    assert_eq!(e.len(), 1, "{e:?}");
    let at = format!("/boot/BITSOS2/home/.papierkorb/{}", e[0]);
    assert_eq!(ns.read_file(&format!("{at}/files/BITSOS2/CONFIG.TXT"), 100).unwrap(), b"screen=1024x768\nnvme_write=off\n");
    let rec = String::from_utf8(ns.read_file(&format!("{at}/record.json"), 4096).unwrap()).unwrap();
    assert!(rec.contains("\"path\":\"/boot/BITSOS2/CONFIG.TXT\""), "{rec}");
    // a rename whose second step goes through: the old file in the trash, the new one at its name
    ns.create_file_bytes(b"/boot/BITSOS2/home/a", 0o644, b"old").unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/home/a.new", 0o644, b"new").unwrap();
    vfs::trash::end_batches_of(7300);
    t.rename(None, b"/boot/BITSOS2/home/a.new", None, b"/boot/BITSOS2/home/a", 0).unwrap();
    assert_eq!(ns.read_file("/boot/BITSOS2/home/a", 100).unwrap(), b"new");
    assert!(ns.lookup("/boot/BITSOS2/home/a.new").is_err());
    assert_eq!(entries(&ns).len(), 2);
    for f in ["/boot/BITSOS2/new.txt", "/boot/BITSOS2/home/a"] {
        ns.lookup(f).unwrap().inode.sync(SyncKind::Fsync).unwrap();
    }
    vol.sync(true).unwrap();
    assert_clean(&dir, &dev.image());
}
