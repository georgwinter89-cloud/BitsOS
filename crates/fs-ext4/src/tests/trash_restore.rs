//! EINS-C (K6, F3): putting entries back and emptying the trash; the write guard and the trash's own rule for
//! the assistant's tree; the control node's commands. e2fsck-clean after each sequence.

use super::trash::{as_proc, chown, entries, files, sync, tree_of, TREE};
use super::util::*;
use super::vfs::fresh_ns;
use std::format;
use std::string::String;
use vfs::abi::{O_CREAT, O_RDONLY, O_RDWR, O_TRUNC, O_WRONLY};
use vfs::inode::Cred;
use vfs::ops::Ctx;
use vfs::trash::Caller;
use uapi::errno::*;

const GEORG: Caller = Caller { pid: 7100, tree: 0, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };

#[test]
fn a_taken_name_refuses_the_restore_until_another_name_is_given() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("a", b"first"), ("d/x", b"x"), ("d/y", b"y")]);
    let t = as_proc(&ns, 7101, 1000, TREE);
    t.unlink(None, b"/data/home/1000/a").unwrap();
    t.unlink(None, b"/data/home/1000/d/x").unwrap();
    t.unlink(None, b"/data/home/1000/d/y").unwrap();
    t.rmdir(None, b"/data/home/1000/d").unwrap();
    let id = entries(&ns).remove(0);
    ns.create_file_bytes(b"/data/home/1000/a", 0o644, b"second").unwrap();
    let held = tree_of(&ns, &format!("/data/.papierkorb/{id}"));
    assert_eq!(vfs::trash::restore(&ns, &GEORG, b"/data", id.as_bytes(), None), Err(EEXIST));
    assert_eq!(tree_of(&ns, &format!("/data/.papierkorb/{id}")), held, "nothing moved");
    assert_eq!(ns.read_file("/data/home/1000/a", 10).unwrap(), b"second");
    assert_eq!(vfs::trash::restore(&ns, &GEORG, b"/data", id.as_bytes(), Some(b"/data/home/1000/a")), Err(EEXIST), "the other name is taken too");
    let r = vfs::trash::restore(&ns, &GEORG, b"/data", id.as_bytes(), Some(b"/data/home/1000/zurueck")).unwrap();
    assert_eq!(r.moved, 1);
    assert_eq!(ns.read_file("/data/home/1000/zurueck/a", 10).unwrap(), b"first");
    assert_eq!(ns.read_file("/data/home/1000/zurueck/d/y", 10).unwrap(), b"y");
    assert!(entries(&ns).is_empty());
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "a restore under another name");
}

#[test]
fn only_georg_outside_the_tree_empties() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("a", b"1"), ("b", b"2")]);
    let t = as_proc(&ns, 7110, 1000, TREE);
    t.unlink(None, b"/data/home/1000/a").unwrap();
    vfs::trash::end_batches_of(7110);
    t.unlink(None, b"/data/home/1000/b").unwrap();
    let mut e = entries(&ns);
    assert_eq!(e.len(), 2);
    // e[0]: the entry with a, e[1]: the one with b
    if ns.lookup(&format!("/data/.papierkorb/{}/files/home/1000/a", e[0])).is_err() {
        e.swap(0, 1);
    }
    let inside = Caller { pid: 7110, tree: TREE, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };
    assert_eq!(vfs::trash::empty(&ns, &inside, b"/data", None), Err(EPERM));
    let stranger = Caller { pid: 7111, tree: 0, cred: Cred { uid: 1001, gid: 1001 }, program: policy::ProgramId(1) };
    assert_eq!(vfs::trash::empty(&ns, &stranger, b"/data", None), Err(EACCES));
    assert_eq!(vfs::trash::restore(&ns, &stranger, b"/data", e[0].as_bytes(), None), Err(EACCES));
    // the tree may put back what it removed
    assert_eq!(vfs::trash::restore(&ns, &inside, b"/data", e[0].as_bytes(), None).map(|r| r.moved), Ok(1));
    assert_eq!(ns.read_file("/data/home/1000/a", 10).unwrap(), b"1");
    // nor may it remove the trash's files itself
    let inner = format!("/data/.papierkorb/{}/files/home/1000/b", e[1]);
    assert_eq!(t.unlink(None, inner.as_bytes()).err(), Some(EPERM));
    assert_eq!(t.rename(None, inner.as_bytes(), None, b"/data/home/1000/b2", 0).err(), Some(EPERM));
    assert_eq!(t.rmdir(None, b"/data/.papierkorb").err(), Some(EPERM));
    assert_eq!(vfs::trash::empty(&ns, &GEORG, b"/data", Some(e[1].as_bytes())), Ok(1));
    assert!(entries(&ns).is_empty());
    assert_eq!(vfs::trash::empty(&ns, &GEORG, b"/data", Some(b"../home")), Err(EINVAL));
    assert_eq!(vfs::trash::empty(&ns, &GEORG, b"/data/home", None), Err(EINVAL), "not a file system's top");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "emptying");
}

/// The places the guard keeps and the control node: in the boot view (`vfs::root_ns`), the one test that
/// mounts there.
#[test]
fn the_tree_never_changes_the_guarded_files_nor_the_trash() {
    let s = Scratch::new();
    let img = mkfs(&s, "16M", &["-b", "4096"], None);
    let dev = std::sync::Arc::new(crate::MemDev::new(img));
    let ns = vfs::root_ns();
    let (vol, _m) = crate::mount::mount_dev(dev.clone(), None, &ns, "/data", opts()).unwrap();
    ns.mkdir_p("/boot/BITSOS2/mcp").unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/CONFIG.TXT", 0o644, b"tz=UTC\n").unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/mcp/server.key", 0o600, b"k").unwrap();
    ns.mkdir_p("/data/bitsos-access").unwrap();
    ns.create_file_bytes(b"/data/bitsos-access/grants", 0o600, b"g").unwrap();
    Ctx::kernel(&ns).symlink(b"/boot/BITSOS2/CONFIG.TXT", None, b"/boot/link").unwrap();
    files(&ns, &[("a", b"1")]);
    let root = as_proc(&ns, 7120, 0, TREE);
    for p in ["/boot/BITSOS2/CONFIG.TXT", "/boot/link", "/boot/BITSOS2/mcp/server.key", "/data/bitsos-access/grants"] {
        assert_eq!(root.open(None, p.as_bytes(), O_WRONLY, 0).err(), Some(EPERM), "write {p}");
        assert_eq!(root.open(None, p.as_bytes(), O_RDONLY | O_TRUNC, 0).err(), Some(EPERM), "truncate {p}");
        assert!(root.open(None, p.as_bytes(), O_RDONLY, 0).is_ok(), "reading {p} stays as the modes say");
    }
    assert_eq!(root.unlink(None, b"/boot/BITSOS2/CONFIG.TXT").err(), Some(EPERM));
    assert_eq!(root.rename(None, b"/boot/BITSOS2/CONFIG.TXT", None, b"/boot/BITSOS2/old.txt", 0).err(), Some(EPERM));
    ns.create_file_bytes(b"/boot/BITSOS2/new.txt", 0o644, b"x").unwrap();
    assert_eq!(root.rename(None, b"/boot/BITSOS2/new.txt", None, b"/boot/BITSOS2/CONFIG.TXT", 0).err(), Some(EPERM));
    assert_eq!(root.truncate(None, b"/boot/BITSOS2/CONFIG.TXT", 0).err(), Some(EPERM));
    assert_eq!(root.open(None, b"/boot/BITSOS2/STICK.ID", O_WRONLY | O_CREAT, 0o644).err(), Some(EPERM));
    assert_eq!(root.open(None, b"/boot/BITSOS2/mcp/paired", O_WRONLY | O_CREAT, 0o600).err(), Some(EPERM));
    assert_eq!(root.mkdir(None, b"/data/bitsos-access/more", 0o700).err(), Some(EPERM));
    assert_eq!(root.rmdir(None, b"/data/bitsos-access").err(), Some(EPERM));
    let p = ns.lookup_path(b"/data/bitsos-access/grants", false).unwrap();
    assert_eq!(root.setattr(&p, |_| vfs::setattr::chmod(0o666)).err(), Some(EPERM));
    assert_eq!(root.open(None, b"/data/.papierkorb", O_WRONLY | O_CREAT, 0o600).err(), Some(EPERM), "nor may it block its own trash");
    assert_eq!(ns.read_file("/boot/BITSOS2/CONFIG.TXT", 100).unwrap(), b"tz=UTC\n");
    // outside the tree all of it is as before
    let georg = as_proc(&ns, 7121, 0, 0);
    drop(georg.open(None, b"/boot/BITSOS2/CONFIG.TXT", O_RDWR, 0).unwrap());
    // the control node, as `bits papierkorb` uses it
    let t = as_proc(&ns, 7122, 1000, TREE);
    t.unlink(None, b"/data/home/1000/a").unwrap();
    let id = entries(&ns).remove(0);
    let inside = Caller { pid: 7122, tree: TREE, cred: Cred { uid: 1000, gid: 1000 }, program: policy::ProgramId(1) };
    assert_eq!(vfs::trash::command(&inside, format!("empty\n/data\n{id}\n").as_bytes()), Err(EPERM));
    // nor may it restore into a guarded place, made new or there already (the kernel would move for it)
    let held = tree_of(&ns, &format!("/data/.papierkorb/{id}"));
    for to in ["/data/bitsos-access/planted", "/data/bitsos-access"] {
        assert_eq!(vfs::trash::command(&inside, format!("restore-to\n/data\n{id}\n{to}\n").as_bytes()), Err(EPERM), "{to}");
    }
    assert_eq!(tree_of(&ns, &format!("/data/.papierkorb/{id}")), held, "the entry is as it was");
    assert!(ns.lookup("/data/bitsos-access/planted").is_err());
    // nor move away a folder that holds a guarded place; making names in it stays free
    ns.mkdir_p("/var/lib/bitsos/settings").unwrap();
    assert_eq!(root.rename(None, b"/var/lib/bitsos", None, b"/var/lib/x", 0).err(), Some(EPERM));
    assert_eq!(root.rename(None, b"/boot/BITSOS2", None, b"/boot/X", 0).err(), Some(EPERM));
    assert_eq!(root.mkdir(None, b"/boot/BITSOS2", 0o755).err(), Some(EEXIST), "mkdir -p still works");
    root.mkdir(None, b"/var/lib/bitsos/other", 0o755).unwrap();
    root.rename(None, b"/var/lib/bitsos/other", None, b"/var/lib/bitsos/other2", 0).unwrap();
    assert_eq!(vfs::trash::command(&GEORG, format!("restore\n/data\n{id}\n").as_bytes()).map(String::from_utf8), Ok(Ok(String::from("moved=1\n"))));
    t.unlink(None, b"/data/home/1000/a").unwrap();
    assert_eq!(vfs::trash::command(&GEORG, b"empty\n/data\nall\n").map(String::from_utf8), Ok(Ok(String::from("emptied=1\n"))));
    assert_eq!(vfs::trash::command(&GEORG, b"tidy\n/data\n"), Err(EINVAL));
    chown(&ns, "/data/home/1000");
    sync(&ns, &vol, "/data");
    ns.umount(&ns.lookup_path(b"/data", true).unwrap()).unwrap();
    assert_clean(&s, &dev.image(), "the guard and the control node");
}

/// Georg removes an open entry by hand (outside the tree): the tree's next removal starts a new one.
#[test]
fn an_entry_removed_by_hand_is_not_used_again() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    files(&ns, &[("a", b"1"), ("b", b"2")]);
    let t = as_proc(&ns, 7130, 1000, TREE);
    t.unlink(None, b"/data/home/1000/a").unwrap();
    let id = entries(&ns).remove(0);
    ns.remove_tree(&format!("/data/.papierkorb/{id}")).unwrap();
    t.unlink(None, b"/data/home/1000/b").unwrap();
    let e = entries(&ns);
    assert_eq!(e.len(), 1);
    assert_eq!(ns.read_file(&format!("/data/.papierkorb/{}/files/home/1000/b", e[0]), 10).unwrap(), b"2");
    sync(&ns, &vol, "/data");
    assert_clean(&s, &dev.image(), "an entry removed by hand");
}
