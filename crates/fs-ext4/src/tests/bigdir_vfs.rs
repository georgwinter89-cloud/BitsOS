//! BIGDIR through the VFS: a lookup that misses a loaded directory and an fstat of a directory need neither
//! `ext4.vol` nor the device (both run here while another thread holds the volume lock), and fstat shows the
//! directory's size and blocks as its inode has them (what Linux ext4 shows) while it grows.

use super::bigdir_dev::{steam_name, CountDev};
use super::util::*;
use crate::mount;
use std::format;
use std::string::String;
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;
use uapi::errno::ENOENT;
use vfs::abi::{O_CREAT, O_DIRECTORY, O_RDONLY, O_RDWR};
use vfs::ops::Ctx;

fn path(i: usize) -> String {
    format!("/data/d/{}", String::from_utf8(steam_name(i, "v")).unwrap())
}

/// The name of file `i` in another case (the folders as they are).
fn other_case(i: usize, upper: bool) -> String {
    let n = String::from_utf8(steam_name(i, "v")).unwrap();
    format!("/data/d/{}", if upper { n.to_ascii_uppercase() } else { n.to_ascii_lowercase() })
}

fn names_listed(c: &Ctx) -> usize {
    let f = c.open(None, b"/data/d", O_RDONLY | O_DIRECTORY, 0).unwrap();
    let mut n = 0;
    loop {
        let b = vfs::ops::read_dir(&f, 32 << 10).unwrap();
        if b.is_empty() {
            return n;
        }
        let mut off = 0;
        while off < b.len() {
            off += u16::from_le_bytes([b[off + 16], b[off + 17]]) as usize;
            n += 1;
        }
    }
}

#[test]
fn a_missed_lookup_and_a_directory_fstat_ask_neither_the_volume_nor_the_device() {
    let s = Scratch::new();
    let dev = CountDev::new(mkfs(&s, "48M", &["-N", "16384"], None));
    let ns = Arc::new(vfs::MountNs::new_empty());
    let (v, _m) = mount::mount_dev(dev.clone(), None, &ns, "/data", opts()).unwrap();
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/data/d", 0o755).unwrap();
    for i in 0..3000 {
        drop(c.open(None, path(i).as_bytes(), O_RDWR | O_CREAT, 0o644).unwrap());
    }
    let a = c.stat(None, b"/data/d", true).unwrap();
    let ino = a.ino as u32;
    let di = v.stat(ino).unwrap();
    assert_eq!((a.size, a.blocks), (di.size(), di.blocks512(&v.geo)), "fstat of the folder against its inode");
    assert!(a.size >= 30 * 4096 && a.size % 4096 == 0);

    // Steam's pattern with the volume lock held elsewhere: the name in another case is not there, then the
    // folder is opened, fstat-ed and listed.
    let reads = dev.reads();
    let walked = v.dirx_counts();
    let held = v.lock();
    let (tx, rx) = mpsc::channel();
    let t = {
        let ns = ns.clone();
        std::thread::spawn(move || {
            let c = Ctx::kernel(&ns);
            for i in 0..3000 {
                assert_eq!(c.access(None, other_case(i, false).as_bytes(), 0, true).err(), Some(ENOENT));
                assert_eq!(c.open(None, other_case(i, true).as_bytes(), O_RDONLY, 0).err(), Some(ENOENT));
                let a2 = c.stat(None, b"/data/d", true).unwrap();
                assert_eq!((a2.size, a2.blocks), (a.size, a.blocks));
                if i % 500 == 0 {
                    assert_eq!(names_listed(&c), 3002);
                }
            }
            tx.send(()).unwrap();
        })
    };
    let done = rx.recv_timeout(Duration::from_secs(60));
    drop(held);
    t.join().unwrap();
    assert!(done.is_ok(), "a missed lookup, an fstat or a listing of a loaded folder waited for ext4.vol");
    assert_eq!(dev.reads(), reads, "a missed lookup, an fstat or a listing of a loaded folder read the device");
    assert_eq!(v.dirx_counts(), walked, "a missed lookup reached ext4's directories");
    // Hardening, finding 2: ext4 confirms "not there" by a walk of the folder before it answers ENOENT to an
    // unlink or rename. That walk is not paid for names that are not there: the VFS answers those from its own
    // map of the loaded folder, as it does a lookup.
    for i in 0..300 {
        assert_eq!(c.unlink(None, other_case(i, false).as_bytes()).err(), Some(ENOENT));
        assert_eq!(c.rmdir(None, other_case(i, true).as_bytes()).err(), Some(ENOENT));
        assert_eq!(c.rename(None, other_case(i, false).as_bytes(), None, b"/data/d/moved", 0).err(), Some(ENOENT));
    }
    assert_eq!((dev.reads(), v.dirx_counts()), (reads, walked), "an unlink, rmdir or rename of a name that is not there reached ext4");

    // The folder grows and shrinks: fstat follows without asking the volume again.
    for i in 3000..9000 {
        drop(c.open(None, path(i).as_bytes(), O_RDWR | O_CREAT, 0o644).unwrap());
    }
    c.rename(None, path(5).as_bytes(), None, b"/data/moved-out", 0).unwrap();
    c.unlink(None, path(6).as_bytes()).unwrap();
    c.mkdir(None, b"/data/d/sub", 0o755).unwrap();
    let held = v.lock();
    let (tx, rx) = mpsc::channel();
    let t = {
        let ns = ns.clone();
        std::thread::spawn(move || tx.send(Ctx::kernel(&ns).stat(None, b"/data/d", true).unwrap()).unwrap())
    };
    let grown = rx.recv_timeout(Duration::from_secs(60));
    let di = held.inode(None, ino).unwrap();
    let want = (di.size(), di.blocks512(&held.geo));
    drop(held);
    t.join().unwrap();
    let grown = grown.expect("fstat of the grown folder waited for ext4.vol");
    assert_eq!((grown.size, grown.blocks), want, "fstat of the grown folder against its inode");
    assert!(grown.size > a.size);
    assert_eq!(names_listed(&c), 9000 - 2 + 1 + 2);
    assert_eq!(v.dirx_check(ino).map(|n| n.is_some()), Ok(true));

    // The indexes are charged to the mount's pool and the charge goes back with them.
    let pool = c.lookup(None, b"/data/d", true).unwrap().dentry.sb.pool.clone();
    let (bytes, n) = v.dirx_bytes();
    let charged = pool.meta_bytes();
    let freed = vfs::meta_shrink::MetaShrink::shrink_meta(&*v, vfs::meta_shrink::Level::All);
    assert!(n >= 1 && bytes > 9000 * 20 && freed >= bytes, "{n} indexes of {bytes} bytes, {freed} given back");
    assert_eq!(pool.meta_bytes(), charged - freed, "the pool's metadata after the indexes went");
    drop(c.open(None, b"/data/d/after-the-shrink", O_RDWR | O_CREAT, 0o644).unwrap());
    c.unlink(None, b"/data/d/after-the-shrink").unwrap();
    assert_eq!(v.dirx_check(ino).map(|n| n.is_some()), Ok(true), "the next change builds the index again");
    v.sync_fs().unwrap();
    assert_clean(&s, &dev.inner.image(), "the folder through the VFS");

    // A fresh mount loads the folder once from the disk; after that a miss reads nothing.
    let dev2 = CountDev::new(dev.inner.image());
    let ns2 = Arc::new(vfs::MountNs::new_empty());
    let (_v2, _m2) = mount::mount_dev(dev2.clone(), None, &ns2, "/data", opts()).unwrap();
    let c2 = Ctx::kernel(&ns2);
    assert_eq!(names_listed(&c2), 9000 - 2 + 1 + 2);
    let b = c2.stat(None, b"/data/d", true).unwrap();
    assert_eq!((b.size, b.blocks), want, "fstat after a fresh mount");
    let reads = dev2.reads();
    for i in 0..2000 {
        assert_eq!(c2.access(None, other_case(i, false).as_bytes(), 0, true).err(), Some(ENOENT));
        c2.stat(None, b"/data/d", true).unwrap();
    }
    assert_eq!(dev2.reads(), reads, "a missed lookup after the load read the device");
}
