//! Reading what mtools wrote, writing what mtools and fsck.fat accept, and POSIX semantics through the VFS.

use super::util::*;
use std::collections::BTreeSet;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;
use std::{format, vec};
use uapi::errno::*;
use vfs::abi::{O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TMPFILE_BIT, O_DIRECTORY, O_TRUNC, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

fn names(ns: &vfs::MountNs, p: &str) -> BTreeSet<String> {
    ns.list_dir(p).unwrap().into_iter().map(|(n, _)| String::from_utf8(n).unwrap()).collect()
}

/// fsync of every file below `p` (dirty pages to the disk), then the volume.
pub fn sync_tree(ns: &vfs::MountNs, p: &str) {
    for (n, a) in ns.list_dir(p).unwrap() {
        let path = format!("{p}/{}", String::from_utf8(n).unwrap());
        if a.mode & uapi::S_IFMT == uapi::S_IFDIR {
            sync_tree(ns, &path);
        } else {
            ns.lookup(&path).unwrap().inode.sync(SyncKind::Fsync).unwrap();
        }
    }
}

#[test]
fn reads_a_tree_made_by_mtools() {
    let dir = scratch("read");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    for d in ["/BITSOS2", "/BITSOS2/bin", "/EFI", "/EFI/BOOT", "/BITSOS2/a", "/BITSOS2/a/b"] {
        mkdir(&img, d);
    }
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("/BITSOS2/KERNEL", pattern(70_000, 1)),
        ("/BITSOS2/Readme.TXT", b"readme\r\n".to_vec()),
        ("/BITSOS2/Länge.txt", b"umlaut".to_vec()),
        ("/BITSOS2/This is a long file name with spaces.txt", pattern(1234, 2)),
        ("/BITSOS2/a/b/deep.bin", pattern(3 << 20, 3)),
        ("/BITSOS2/empty", Vec::new()),
        ("/EFI/BOOT/BOOTX64.EFI", pattern(5000, 4)),
    ];
    for (p, d) in &files {
        put_bytes(&img, &dir, p, d);
    }
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    assert_eq!(names(&ns, "/boot"), ["BITSOS2", "EFI"].iter().map(|s| String::from(*s)).collect());
    let want: BTreeSet<String> =
        ["KERNEL", "Readme.TXT", "Länge.txt", "This is a long file name with spaces.txt", "a", "bin", "empty"].iter().map(|s| String::from(*s)).collect();
    assert_eq!(names(&ns, "/boot/BITSOS2"), want);
    for (p, d) in &files {
        let got = ns.read_file(&format!("/boot{p}"), 8 << 20).unwrap();
        assert!(got == *d, "{p}: {} bytes, want {}", got.len(), d.len());
    }
    // Case-insensitive, trailing dots ignored, and by the 8.3 alias (Linux vfat finds all three).
    let a = ns.lookup("/boot/bitsos2/README.txt").unwrap();
    let b = ns.lookup("/boot/BITSOS2/readme.txt.").unwrap();
    assert!(Arc::ptr_eq(&a.inode, &b.inode));
    let alias = {
        let st = vol.st.lock();
        let n = st.nodes.values().find(|n| n.name == "This is a long file name with spaces.txt".as_bytes()).unwrap();
        String::from_utf8(crate::name::short_key(&n.e.name)).unwrap()
    };
    assert!(alias.contains('~'), "{alias}");
    let c = ns.lookup(&format!("/boot/BITSOS2/{alias}")).unwrap();
    assert_eq!(c.name(), "This is a long file name with spaces.txt".as_bytes());
    assert_eq!(ns.lookup("/boot/BITSOS2/missing").err(), Some(ENOENT));
    // Reading changed nothing.
    assert_eq!(dev.image(), std::fs::read(&img).unwrap());
}

#[test]
fn writes_that_linux_reads_back() {
    let dir = scratch("write");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    mkdir(&img, "/BITSOS2");
    put_bytes(&img, &dir, "/BITSOS2/old.txt", &pattern(5000, 9));
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let long200 = format!("{}.dat", "n".repeat(196));
    let mut expect: Vec<(String, Vec<u8>)> = Vec::new();
    for (name, len, seed) in [
        ("zero", 0usize, 0u32),
        ("one byte", 1, 1),
        ("SECTOR.BIN", 512, 2),
        ("page.bin", 4096, 3),
        ("Größe ändern.txt", 4097, 4),
        ("x.y.z.tar.gz", 70_000, 5),
        ("big file", 1 << 20, 6),
        (&long200, 777, 7),
    ] {
        let path = format!("/boot/BITSOS2/new dir/sub/{name}");
        let data = pattern(len, seed);
        ns.create_file_bytes(path.as_bytes(), 0o644, &data).unwrap();
        expect.push((format!("/BITSOS2/new dir/sub/{name}"), data));
    }
    // Append, write past the end (a gap), overwrite in the middle.
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/boot/BITSOS2/old.txt", O_RDWR, 0).unwrap();
    let mut old = pattern(5000, 9);
    f.pwrite(100, b"HELLO").unwrap();
    old[100..105].copy_from_slice(b"HELLO");
    f.pwrite(20_000, b"tail").unwrap();
    old.resize(20_000, 0);
    old.extend_from_slice(b"tail");
    expect.push((String::from("/BITSOS2/old.txt"), old));
    // Truncate down and up.
    let t = format!("/boot/BITSOS2/new dir/sub/big file");
    c.truncate(None, t.as_bytes(), 3000).unwrap();
    c.truncate(None, t.as_bytes(), 9000).unwrap();
    let mut big = pattern(1 << 20, 6);
    big.truncate(3000);
    big.resize(9000, 0);
    expect.iter_mut().find(|(p, _)| p.ends_with("big file")).unwrap().1 = big;
    // Rename and remove.
    c.rename(None, b"/boot/BITSOS2/new dir/sub/one byte", None, b"/boot/BITSOS2/moved here", 0).unwrap();
    let one = expect.iter().position(|(p, _)| p.ends_with("one byte")).unwrap();
    let (_, d) = expect.remove(one);
    expect.push((String::from("/BITSOS2/moved here"), d));
    c.unlink(None, b"/boot/BITSOS2/new dir/sub/page.bin").unwrap();
    expect.retain(|(p, _)| !p.ends_with("page.bin"));
    c.mkdir(None, b"/boot/BITSOS2/gone", 0o755).unwrap();
    c.rmdir(None, b"/boot/BITSOS2/gone").unwrap();
    sync_tree(&ns, "/boot");
    // Through the VFS ...
    for (p, d) in &expect {
        assert!(ns.read_file(&format!("/boot{p}"), 4 << 20).unwrap() == *d, "{p} via the VFS");
    }
    // ... and what Linux's tools see.
    let after = save(&dir, &dev);
    assert_clean(&dir, &dev.image());
    for (p, d) in &expect {
        let got = get(&after, p).unwrap_or_else(|| panic!("{p}: mtype cannot read it"));
        assert!(got == *d, "{p}: mtools reads {} bytes, want {}", got.len(), d.len());
    }
    let mut want: Vec<String> = ["zero", "SECTOR.BIN", "Größe ändern.txt", "x.y.z.tar.gz", "big file", &long200].iter().map(|s| String::from(*s)).collect();
    want.sort();
    let mut got = ls(&after, "/BITSOS2/new dir/sub");
    got.sort();
    assert_eq!(got, want);
    assert!(get(&after, "/BITSOS2/gone").is_none());
    assert!(dev.flushes() > 0, "fsync reached the device (SYNCHRONIZE CACHE)");
}

#[test]
fn a_directory_grows_past_many_clusters() {
    let dir = scratch("grow");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/boot/many", 0o755).unwrap();
    let mut want = Vec::new();
    for i in 0..300 {
        let n = format!("A file with a long name number {i:03}.txt");
        ns.create_file_bytes(format!("/boot/many/{n}").as_bytes(), 0o644, n.as_bytes()).unwrap();
        want.push(n);
    }
    // Remove every third, add some more: deleted slots are reused.
    for i in (0..300).step_by(3) {
        let n = format!("A file with a long name number {i:03}.txt");
        c.unlink(None, format!("/boot/many/{n}").as_bytes()).unwrap();
        want.retain(|x| *x != n);
    }
    for i in 0..50 {
        let n = format!("again {i}");
        ns.create_file_bytes(format!("/boot/many/{n}").as_bytes(), 0o644, b"x").unwrap();
        want.push(n);
    }
    sync_tree(&ns, "/boot");
    assert_clean(&dir, &dev.image());
    let after = save(&dir, &dev);
    let mut got = ls(&after, "/many");
    got.sort();
    want.sort();
    assert_eq!(got, want);
    assert_eq!(get(&after, "/many/A file with a long name number 299.txt").unwrap(), b"A file with a long name number 299.txt");
    // A fresh mount reads the same.
    let vol2 = mount_plain(&dev);
    let ns2 = view(&vol2);
    assert_eq!(names(&ns2, "/boot/many").len(), want.len());
}

#[test]
fn posix_semantics() {
    let dir = scratch("posix");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let free0 = vol.statfs().unwrap().2;
    // Unlink of an open file: still readable and writable, its clusters go at the last close.
    ns.create_file_bytes(b"/boot/open.bin", 0o644, &pattern(100_000, 1)).unwrap();
    ns.lookup("/boot/open.bin").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    let f = c.open(None, b"/boot/open.bin", O_RDWR, 0).unwrap();
    c.unlink(None, b"/boot/open.bin").unwrap();
    assert_eq!(ns.lookup("/boot/open.bin").err(), Some(ENOENT));
    let mut b = vec![0u8; 100_000];
    assert_eq!(f.pread(0, &mut b).unwrap(), 100_000);
    assert!(b == pattern(100_000, 1));
    f.pwrite(0, b"still writable").unwrap();
    f.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    let mut h = [0u8; 14];
    f.pread(0, &mut h).unwrap();
    assert_eq!(&h, b"still writable");
    assert!(vol.statfs().unwrap().2 < free0, "the open file still holds its clusters");
    drop(f);
    vol.sync(true).unwrap();
    assert_eq!(vol.statfs().unwrap().2, free0, "clusters back after the last close");
    assert_clean(&dir, &dev.image());

    // Rename over an existing name: a reader of the old target keeps its content.
    ns.create_file_bytes(b"/boot/target", 0o644, b"old content").unwrap();
    ns.create_file_bytes(b"/boot/source", 0o644, b"new content").unwrap();
    sync_tree(&ns, "/boot");
    let old = c.open(None, b"/boot/target", O_RDONLY, 0).unwrap();
    c.rename(None, b"/boot/source", None, b"/boot/target", 0).unwrap();
    assert_eq!(ns.read_file("/boot/target", 100).unwrap(), b"new content");
    let mut o = [0u8; 11];
    old.pread(0, &mut o).unwrap();
    assert_eq!(&o, b"old content");
    drop(old);
    vol.sync(true).unwrap();
    let after = save(&dir, &dev);
    assert_eq!(get(&after, "/target").unwrap(), b"new content");
    assert!(get(&after, "/source").is_none());
    assert_clean(&dir, &dev.image());

    // Names: case-insensitive EEXIST, and what vfat has no method for.
    assert_eq!(c.open(None, b"/boot/TARGET", O_WRONLY | O_CREAT | O_EXCL, 0o644).err(), Some(EEXIST));
    assert_eq!(c.mkdir(None, b"/boot/Target", 0o755).err(), Some(EEXIST));
    assert_eq!(c.symlink(b"x", None, b"/boot/link").err(), Some(EPERM));
    assert_eq!(c.link(None, b"/boot/target", None, b"/boot/hard", false).err(), Some(EPERM));
    assert_eq!(c.open(None, b"/boot", O_WRONLY | O_TMPFILE_BIT | O_DIRECTORY, 0o644).err(), Some(EOPNOTSUPP));
    assert_eq!(c.open(None, b"/boot/bad:name", O_WRONLY | O_CREAT, 0o644).err(), Some(EINVAL));
    assert_eq!(c.open(None, "/boot/x".repeat(1).as_bytes(), O_WRONLY | O_CREAT | O_TRUNC, 0o644).map(|_| ()), Ok(()));

    // mtime: a write moves it, on the disk too.
    let before = ns.lookup("/boot/target").unwrap().attr().mtime;
    let t = c.open(None, b"/boot/target", O_WRONLY, 0).unwrap();
    t.pwrite(0, b"N").unwrap();
    t.inode().unwrap().sync(SyncKind::Fsync).unwrap();
    let now = ns.lookup("/boot/target").unwrap().attr().mtime;
    assert!(now.sec >= before.sec);
    let st = vol.st.lock();
    let n = st.nodes.values().find(|n| n.name == b"target" && n.linked).unwrap();
    assert_ne!(n.e.wdate, 0, "the entry carries a write date");
    drop(st);

    // A directory moved to another parent: ".." follows.
    c.mkdir(None, b"/boot/d1", 0o755).unwrap();
    c.mkdir(None, b"/boot/d2", 0o755).unwrap();
    c.mkdir(None, b"/boot/d1/inner", 0o755).unwrap();
    ns.create_file_bytes(b"/boot/d1/inner/f", 0o644, b"f").unwrap();
    c.rename(None, b"/boot/d1/inner", None, b"/boot/d2/inner", 0).unwrap();
    sync_tree(&ns, "/boot");
    assert_clean(&dir, &dev.image());
    let after = save(&dir, &dev);
    assert_eq!(get(&after, "/d2/inner/f").unwrap(), b"f");
    // Removing a non-empty directory.
    assert_eq!(c.rmdir(None, b"/boot/d2").err(), Some(ENOTEMPTY));
}

#[test]
fn errors_and_read_ahead() {
    let dir = scratch("eio");
    let img = mkfs(&dir, 40, 1, "BITSOS");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    // Read-ahead never shows stale bytes: shrink, grow again, the tail reads as zeros.
    let data = pattern(300_000, 11);
    let n = ns.create_file_bytes(b"/boot/ra.bin", 0o644, &data).unwrap();
    n.inode.sync(SyncKind::Fsync).unwrap();
    drop(n);
    let vol2 = mount_plain(&dev);
    let ns2 = view(&vol2);
    assert!(ns2.read_file("/boot/ra.bin", 1 << 20).unwrap() == data, "read through the read-ahead");
    let c2 = Ctx::kernel(&ns2);
    let f = c2.open(None, b"/boot/ra.bin", O_RDWR, 0).unwrap();
    let mut b = vec![0u8; 4096];
    f.pread(0, &mut b).unwrap();
    c2.truncate(None, b"/boot/ra.bin", 5000).unwrap();
    c2.truncate(None, b"/boot/ra.bin", 300_000).unwrap();
    let back = ns2.read_file("/boot/ra.bin", 1 << 20).unwrap();
    assert!(back[..5000] == data[..5000] && back[5000..].iter().all(|&x| x == 0), "a grown file reads zeros past its old end");
    drop(f);
    // A device error is an errno, never a panic, and the pages stay dirty for the next try.
    ns.create_file_bytes(b"/boot/later.bin", 0o644, &pattern(20_000, 12)).unwrap();
    dev.fail_writes(true);
    let node = ns.lookup("/boot/later.bin").unwrap();
    assert_eq!(node.inode.sync(SyncKind::Fsync).err(), Some(EIO));
    assert_eq!(c.mkdir(None, b"/boot/nodir", 0o755).err(), Some(EIO));
    dev.fail_writes(false);
    node.inode.sync(SyncKind::Fsync).unwrap();
    vol.sync(true).unwrap();
    let vol3 = mount_plain(&dev);
    let ns3 = view(&vol3);
    assert!(ns3.read_file("/boot/later.bin", 1 << 20).unwrap() == pattern(20_000, 12));
    c.mkdir(None, b"/boot/sub", 0o755).unwrap();
    ns.create_file_bytes(b"/boot/sub/x", 0o644, b"x").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    let ns4 = view(&mount_plain(&dev));
    dev.fail_reads(true);
    assert_eq!(ns4.lookup("/boot/sub/x").err(), Some(EIO), "a directory that cannot be read");
    dev.fail_reads(false);
    assert!(ns4.lookup("/boot/sub/x").is_ok(), "the next lookup loads it");
}

#[test]
fn background_write_back_makes_the_stick_durable() {
    let dir = scratch("bg");
    // Its pass reaches every registered file of the process: no other test may be running then.
    let _alone = exclusive();
    let img = mkfs(&dir, 40, 1, "BITSOS");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let w: std::sync::Weak<dyn vfs::writeback::Flush> = Arc::downgrade(&vol) as std::sync::Weak<dyn vfs::writeback::Flush>;
    vfs::writeback::register_fs(w).unwrap();
    let ns = view(&vol);
    let data = pattern(50_000, 21);
    ns.create_file_bytes(b"/boot/no fsync.bin", 0o644, &data).unwrap();
    Ctx::kernel(&ns).mkdir(None, b"/boot/a dir", 0o755).unwrap();
    let f0 = dev.flushes();
    vfs::writeback::flush_all(SyncKind::Background);
    assert!(dev.flushes() > f0, "the pass ended with the device's cache flush");
    assert_clean(&dir, &dev.image());
    let after = save(&dir, &dev);
    assert!(get(&after, "/no fsync.bin").unwrap() == data);
    // Nothing changed since: the next pass sends no flush.
    let f1 = dev.flushes();
    vol.flush_if_changed().unwrap();
    assert_eq!(dev.flushes(), f1);
}
