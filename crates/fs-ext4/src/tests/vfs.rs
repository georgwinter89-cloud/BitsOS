//! Through the VFS: an ext4 volume mounted in a view (and as a loop image file on tmpfs), driven by the
//! kernel's file operations; e2fsck after, and a second mount reads it back with the same inode numbers.

use super::util::*;
use crate::mount;
use std::string::String;
use std::sync::Arc;
use std::vec::Vec;
use std::{format, vec};
use vfs::abi::{O_CREAT, O_RDWR, O_WRONLY};
use vfs::ops::Ctx;
use vfs::pagecache::SyncKind;

fn sync_tree(ns: &vfs::MountNs, p: &str) {
    for (n, a) in ns.list_dir(p).unwrap() {
        let path = format!("{p}/{}", String::from_utf8(n).unwrap());
        if a.mode & uapi::S_IFMT == uapi::S_IFDIR {
            sync_tree(ns, &path);
        } else if a.mode & uapi::S_IFMT == uapi::S_IFREG {
            ns.lookup(&path).unwrap().inode.sync(SyncKind::Fsync).unwrap();
        }
    }
}

fn work(ns: &vfs::MountNs) -> u64 {
    let c = Ctx::kernel(ns);
    c.mkdir(None, b"/data/profile", 0o700).unwrap();
    c.mkdir(None, b"/data/profile/Cache", 0o700).unwrap();
    for i in 0..300 {
        ns.create_file_bytes(format!("/data/profile/Cache/f_{i:06x}").as_bytes(), 0o600, &vec![i as u8; 100 + i * 37]).unwrap();
    }
    let f = c.open(None, b"/data/profile/History", O_RDWR | O_CREAT, 0o600).unwrap();
    f.pwrite(0, &vec![0xAB; 70_000]).unwrap();
    f.pwrite(1 << 20, b"page at 1 MiB").unwrap();
    f.pwrite(10, b"overwrite").unwrap();
    let ino = c.stat(None, b"/data/profile/History", true).unwrap().ino;
    c.symlink(b"History", None, b"/data/profile/SingletonLock").unwrap();
    c.symlink("t/".repeat(70).as_bytes(), None, b"/data/profile/long-link").unwrap();
    c.mknod(None, b"/data/profile/fifo", uapi::S_IFIFO | 0o600, 0).unwrap();
    c.link(None, b"/data/profile/History", None, b"/data/profile/History-hard", false).unwrap();
    c.rename(None, b"/data/profile/Cache/f_000001", None, b"/data/profile/renamed", 0).unwrap();
    c.rename(None, b"/data/profile/Cache/f_000002", None, b"/data/profile/renamed", 0).unwrap();
    // unlink while open: the data stays readable through the open file
    let g = c.open(None, b"/data/profile/Cache/f_000003", O_RDWR, 0).unwrap();
    c.unlink(None, b"/data/profile/Cache/f_000003").unwrap();
    let mut b = vec![0u8; 50];
    assert_eq!(g.pread(0, &mut b).unwrap(), 50);
    assert!(b.iter().all(|&x| x == 3));
    drop(g);
    c.truncate(None, b"/data/profile/Cache/f_000004", 5).unwrap();
    let w = c.open(None, b"/data/profile/new", O_WRONLY | O_CREAT, 0o644).unwrap();
    w.pwrite(0, b"fresh").unwrap();
    drop(w);
    c.rmdir(None, b"/data/profile/Cache").unwrap_err();
    sync_tree(ns, "/data");
    ino
}

fn check(ns: &vfs::MountNs, ino: u64) {
    let c = Ctx::kernel(ns);
    let a = c.stat(None, b"/data/profile/History", true).unwrap();
    assert_eq!(a.ino, ino, "stable inode number");
    assert_eq!(a.size, (1 << 20) + 13);
    assert_eq!(a.nlink, 2);
    assert!(a.blocks >= 70_000 / 512 && a.blocks < 1000, "{} sectors", a.blocks);
    let d = ns.read_file("/data/profile/History", 2 << 20).unwrap();
    assert_eq!(&d[10..19], b"overwrite");
    assert_eq!(d[70_000], 0);
    assert_eq!(&d[1 << 20..], b"page at 1 MiB");
    assert_eq!(ns.read_link("/data/profile/SingletonLock").unwrap(), b"History");
    assert_eq!(ns.read_link("/data/profile/long-link").unwrap(), "t/".repeat(70).as_bytes());
    assert_eq!(c.stat(None, b"/data/profile/fifo", false).unwrap().mode & uapi::S_IFMT, uapi::S_IFIFO);
    assert_eq!(ns.read_file("/data/profile/renamed", 1000).unwrap(), vec![2u8; 100 + 2 * 37]);
    assert_eq!(ns.read_file("/data/profile/Cache/f_000004", 1000).unwrap(), vec![4u8; 5]);
    assert_eq!(ns.read_file("/data/profile/new", 100).unwrap(), b"fresh");
    assert_eq!(ns.list_dir("/data/profile/Cache").unwrap().len(), 300 - 3);
    let st = vfs::statfs::of(&ns.lookup("/data").unwrap().sb, false).unwrap();
    assert_eq!(st.ftype, 0xEF53);
    assert!(st.bfree > 0 && st.bfree < st.blocks);
}

#[test]
fn through_the_vfs() {
    let s = Scratch::new();
    let img = mkfs(&s, "64M", &[], None);
    let dev = Arc::new(crate::MemDev::new(img));
    let ns = vfs::MountNs::new_empty();
    let (vol, _m) = mount::mount_dev(dev.clone(), None, &ns, "/data", opts()).unwrap();
    let ino = work(&ns);
    check(&ns, ino);
    vol.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "VFS work");
    let ns2 = vfs::MountNs::new_empty();
    mount::mount_dev(Arc::new(crate::MemDev::new(dev.image())), None, &ns2, "/data", opts()).unwrap();
    check(&ns2, ino);
}

#[test]
fn loop_image_on_another_file_system() {
    let s = Scratch::new();
    let img = mkfs(&s, "32M", &["-b", "4096"], None);
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/boot/BITSOS2").unwrap();
    ns.create_file_bytes(b"/boot/BITSOS2/data.img", 0o644, &img).unwrap();
    let (vol, _m) = mount::mount_image(&ns, "/boot/BITSOS2/data.img", "/data", opts()).unwrap();
    let ino = work(&ns);
    check(&ns, ino);
    mount::sync_all().unwrap();
    let after: Vec<u8> = ns.read_file("/boot/BITSOS2/data.img", 64 << 20).unwrap();
    assert_eq!(after.len(), img.len());
    assert_clean(&s, &after, "work on a loop image");
    assert!(mount::mounted().iter().any(|v| Arc::ptr_eq(v, &vol)));
}

#[test]
fn one_image_is_mounted_once() {
    let s = Scratch::new();
    let img = mkfs(&s, "16M", &["-b", "4096"], None);
    let ns = vfs::MountNs::new_empty();
    ns.mkdir_p("/boot").unwrap();
    ns.create_file_bytes(b"/boot/data.img", 0o644, &img).unwrap();
    ns.mkdir_p("/boot/again").unwrap();
    Ctx::kernel(&ns).link(None, b"/boot/data.img", None, b"/boot/again/same.img", false).unwrap();
    let first = mount::mount_image(&ns, "/boot/data.img", "/data", opts()).unwrap();
    assert_eq!(mount::mount_image(&ns, "/boot/data.img", "/other", opts()).err(), Some(crate::errno::EBUSY));
    assert_eq!(mount::mount_image(&ns, "/boot/again/same.img", "/other", opts()).err(), Some(crate::errno::EBUSY));
    let ro = crate::vol::Options { writable: false, ..opts() };
    assert_eq!(mount::mount_image(&ns, "/boot/data.img", "/other", ro).err(), Some(crate::errno::EBUSY));
    ns.umount(&ns.lookup_path(b"/data", true).unwrap()).unwrap();
    drop(first);
    mount::mount_image(&ns, "/boot/data.img", "/data", opts()).unwrap();
}

fn fresh_ns(s: &Scratch, size: &str, extra: &[&str]) -> (Arc<crate::MemDev>, vfs::MountNs, Arc<crate::Volume>, Arc<vfs::mount_ns::Mount>) {
    let img = mkfs(s, size, extra, None);
    let dev = Arc::new(crate::MemDev::new(img));
    let ns = vfs::MountNs::new_empty();
    let (vol, m) = mount::mount_dev(dev.clone(), None, &ns, "/data", opts()).unwrap();
    (dev, ns, vol, m)
}

/// `inode_init_owner`: a set-group-ID directory gives new nodes its group and new directories its S_ISGID;
/// a group-executable S_ISGID file of a creator outside that group loses S_ISGID (`mode_strip_sgid`).
#[test]
fn setgid_directory_hands_down_its_group() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    let k = Ctx::kernel(&ns);
    k.mkdir(None, b"/data/shared", 0o777).unwrap();
    let p = ns.lookup_path(b"/data/shared", true).unwrap();
    k.setattr(&p, |m| vfs::setattr::chown(m, vfs::inode::Cred::ROOT, u32::MAX, 50)).unwrap();
    k.setattr(&p, |_| vfs::setattr::chmod(0o2777)).unwrap();
    let mut u = Ctx::kernel(&ns);
    u.cred = vfs::inode::Cred { uid: 1000, gid: 1000 };
    u.mkdir(None, b"/data/shared/sub", 0o755).unwrap();
    drop(u.open(None, b"/data/shared/f", O_WRONLY | O_CREAT, 0o2755).unwrap());
    let d = u.stat(None, b"/data/shared/sub", true).unwrap();
    assert_eq!((d.gid, d.mode & 0o7777), (50, 0o2755));
    // a directory's st_size and st_blocks are its disk blocks, as Linux ext4 shows them
    assert_eq!((d.size, d.blocks), (4096, 8));
    let f = u.stat(None, b"/data/shared/f", true).unwrap();
    assert_eq!((f.gid, f.mode & 0o7777), (50, 0o755));
    sync_tree(&ns, "/data");
    vol.sync_fs().unwrap();
    let (_d2, v2) = mount(dev.image());
    let sub = v2.stat(lookup(&v2, "/shared/sub")).unwrap();
    assert_eq!((sub.gid(), sub.mode() & 0o7777), (50, 0o2755));
    assert_clean(&s, &dev.image(), "setgid directory");
}

/// A dirty partial last page cut down and grown again while still dirty: the bytes past the cut read zero,
/// before and after write-back and after a fresh mount.
#[test]
fn truncate_of_a_dirty_partial_page() {
    let s = Scratch::new();
    for bs in ["1024", "4096"] {
        let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", bs]);
        let c = Ctx::kernel(&ns);
        let f = c.open(None, b"/data/f", O_RDWR | O_CREAT, 0o644).unwrap();
        f.pwrite(0, &vec![0xAB; 10_000]).unwrap();
        // once with the data on disk already, once with it only in the cache
        for round in 0..2 {
            if round == 1 {
                f.pwrite(0, &vec![0xCD; 10_000]).unwrap();
            } else {
                ns.lookup("/data/f").unwrap().inode.sync(SyncKind::Fsync).unwrap();
                f.pwrite(5000, &vec![0xEE; 3000]).unwrap();
            }
            let fill = if round == 0 { 0xAB } else { 0xCD };
            c.truncate(None, b"/data/f", 5000).unwrap();
            c.truncate(None, b"/data/f", 9000).unwrap();
            let mut b = vec![1u8; 9000];
            assert_eq!(f.pread(0, &mut b).unwrap(), 9000);
            assert!(b[..5000].iter().all(|&x| x == fill), "bs {bs} round {round}");
            assert!(b[5000..].iter().all(|&x| x == 0), "bs {bs} round {round}: stale bytes past the cut");
            ns.lookup("/data/f").unwrap().inode.sync(SyncKind::Fsync).unwrap();
            vol.sync_fs().unwrap();
            let (_d2, v2) = mount(dev.image());
            let mut b2 = vec![1u8; 9000];
            assert_eq!(v2.read(lookup(&v2, "/f"), 0, &mut b2).unwrap(), 9000);
            assert_eq!(b, b2, "bs {bs} round {round}: disk differs from the cache");
        }
        assert_clean(&s, &dev.image(), "truncate of a dirty page");
    }
}

/// The first file past 2 GiB on a volume without large_file sets the feature (`ext4_do_update_inode`).
#[test]
fn large_file_feature_is_set_when_needed() {
    let s = Scratch::new();
    // mkfs.ext4 always sets large_file (and the resize inode counts as a large file): cleared afterwards.
    let img = mkfs(&s, "64M", &["-b", "4096", "-O", "^resize_inode,^huge_file"], None);
    let p = s.path("nolarge.img");
    std::fs::write(&p, &img).unwrap();
    assert!(std::process::Command::new("debugfs").args(["-w", "-R", "feature -large_file"]).arg(&p).output().unwrap().status.success());
    let img = std::fs::read(&p).unwrap();
    assert!(!debugfs(&s, &img, "features").contains("large_file"));
    assert_clean(&s, &img, "mkfs without large_file");
    let (dev, v) = mount(img);
    let n = v.create(crate::inode::ROOT, b"big", 0o100644, 0, 0, crate::Kind::Reg).unwrap();
    v.write(n.ino, 3 << 30, &[7u8; 4096], (3 << 30) + 4096, 0..0).unwrap();
    v.sync_fs().unwrap();
    let img = dev.image();
    assert!(debugfs(&s, &img, "features").contains("large_file"));
    assert_clean(&s, &img, "a 3 GiB sparse file without large_file at mkfs");
}

/// fallocate makes unwritten extents on the disk: st_blocks grows, the range reads zero, the space is still
/// there after a fresh mount, and writing into it later converts it (`ext4_fallocate`).
#[test]
fn fallocate_allocates_unwritten_extents() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "32M", &["-b", "4096"]);
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/data/pre", O_RDWR | O_CREAT, 0o644).unwrap();
    f.pwrite(0, b"head").unwrap();
    let node = ns.lookup("/data/pre").unwrap().inode.clone();
    node.fallocate(vfs::abi::FALLOC_FL_KEEP_SIZE, 0, 4 << 20).unwrap();
    node.fallocate(0, 4 << 20, 1 << 20).unwrap();
    let a = c.stat(None, b"/data/pre", true).unwrap();
    assert_eq!(a.size, 5 << 20);
    assert!(a.blocks >= (5 << 20) / 512, "st_blocks {}", a.blocks);
    let mut b = vec![1u8; 4096];
    f.pread(1 << 20, &mut b).unwrap();
    assert!(b.iter().all(|&x| x == 0));
    node.sync(SyncKind::Fsync).unwrap();
    vol.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "fallocate");
    let (_d2, v2) = mount(dev.image());
    let ino = lookup(&v2, "/pre");
    assert!(v2.stat(ino).unwrap().blocks512(&v2.geo) >= (5 << 20) / 512, "the space survives a remount");
    f.pwrite(2 << 20, &vec![9u8; 8192]).unwrap();
    node.sync(SyncKind::Fsync).unwrap();
    vol.sync_fs().unwrap();
    let img = dev.image();
    assert_clean(&s, &img, "write into fallocated space");
    let (_d3, v3) = mount(img);
    let mut r = vec![0u8; 8192 + 8];
    v3.read(lookup(&v3, "/pre"), (2 << 20) - 4, &mut r).unwrap();
    assert!(r[..4].iter().all(|&x| x == 0) && r[4..8196].iter().all(|&x| x == 9) && r[8196..].iter().all(|&x| x == 0));
    let mut h = [0u8; 4];
    v3.read(lookup(&v3, "/pre"), 0, &mut h).unwrap();
    assert_eq!(&h, b"head");
}

std::thread_local!(static AS: std::cell::Cell<Option<vfs::inode::Cred>> = const { std::cell::Cell::new(None) });
fn test_cred() -> Option<vfs::inode::Cred> {
    AS.with(|a| a.get())
}

/// The root reserve (`s_r_blocks_count`): a user's writes stop at ENOSPC before it, root's go on into it
/// (`ext4_has_free_clusters`); what root wrote there is written back and e2fsck-clean.
#[test]
fn root_reserve_is_for_root() {
    vfs::inode::set_current_cred(test_cred);
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "32M", &["-b", "4096", "-m", "10"]);
    let c = Ctx::kernel(&ns);
    let fill = |name: &[u8]| -> u64 {
        let f = c.open(None, name, O_RDWR | O_CREAT, 0o644).unwrap();
        let chunk = vec![0x42u8; 256 << 10];
        let mut at = 0u64;
        loop {
            match f.pwrite(at, &chunk) {
                Ok(n) => at += n as u64,
                Err(e) => {
                    assert_eq!(e, crate::errno::ENOSPC);
                    return at;
                }
            }
        }
    };
    AS.with(|a| a.set(Some(vfs::inode::Cred { uid: 1000, gid: 1000 })));
    let user = fill(b"/data/user");
    let st = vol.statfs().unwrap();
    assert!(st.bavail < 64, "user stopped with bavail {}", st.bavail);
    assert!(st.bfree > 600, "the reserve is still free: bfree {}", st.bfree);
    AS.with(|a| a.set(Some(vfs::inode::Cred::ROOT)));
    let root = fill(b"/data/root");
    AS.with(|a| a.set(None));
    assert!(user > 20 << 20 && root > 2 << 20, "user {user} root {root}");
    sync_tree(&ns, "/data");
    vol.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "root wrote into the reserve");
    let (_d2, v2) = mount(dev.image());
    assert_eq!(v2.stat(lookup(&v2, "/root")).unwrap().size(), root);
}

/// B1/VS-2 through the VFS: `FsFreeze::freeze` of the mounted ext4 writes the dirty pages back, commits and
/// leaves an image that is clean without a replay (its data readable by debugfs); a write and a new name
/// wait until the thaw, then go through.
#[test]
fn freeze_through_the_vfs() {
    use vfs::FsFreeze;
    let s = Scratch::new();
    let (dev, ns, _vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    let c = Ctx::kernel(&ns);
    let f = c.open(None, b"/data/cached", O_RDWR | O_CREAT, 0o644).unwrap();
    f.pwrite(0, &vec![0x77; 10_000]).unwrap();
    let sb = ns.lookup_path(b"/data", true).unwrap().dentry.sb.clone();
    sb.freeze().unwrap();
    let img = dev.image();
    assert_eq!(crate::le::g32(&img, 1024 + crate::sb::FEATURE_INCOMPAT) & crate::sb::incompat::RECOVER, 0);
    assert_clean(&s, &img, "frozen through the VFS");
    let p = s.path("frozen.img");
    std::fs::write(&p, &img).unwrap();
    let cat = std::process::Command::new("debugfs").args(["-R", "cat /cached"]).arg(&p).output().unwrap();
    assert!(cat.stdout == vec![0x77; 10_000], "the dirty pages were not written back ({} bytes)", cat.stdout.len());
    let ns = Arc::new(ns);
    let n2 = ns.clone();
    let t = std::thread::spawn(move || {
        let c = Ctx::kernel(&n2);
        c.mkdir(None, b"/data/during", 0o755).map(|_| ())
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!t.is_finished(), "a new name went through a frozen file system");
    assert!(dev.image() == img, "the frozen volume wrote");
    sb.thaw();
    t.join().unwrap().unwrap();
    drop(f);
    sync_tree(&ns, "/data");
    _vol.sync_fs().unwrap();
    assert_clean(&s, &dev.image(), "after the thaw");
}

/// auto_da_alloc (linux-6.6/fs/ext4/namei.c:3871, VS-2 CR4): a file written and renamed over an older one has
/// its data on disk when the rename commits, so a cut right after the commit (the volume synced, the page cache
/// never written back) finds the new content under the old name, not 0 bytes.
#[test]
fn rename_over_writes_the_data_first() {
    let s = Scratch::new();
    let (dev, ns, vol, _m) = fresh_ns(&s, "16M", &["-b", "4096"]);
    let c = Ctx::kernel(&ns);
    let old = c.open(None, b"/data/saved", O_WRONLY | O_CREAT, 0o600).unwrap();
    old.pwrite(0, &vec![0x11; 9000]).unwrap();
    drop(old);
    ns.lookup("/data/saved").unwrap().inode.sync(SyncKind::Fsync).unwrap();
    let new = c.open(None, b"/data/saved.tmp", O_WRONLY | O_CREAT, 0o600).unwrap();
    new.pwrite(0, &vec![0x22; 12_345]).unwrap();
    drop(new);
    c.rename(None, b"/data/saved.tmp", None, b"/data/saved", 0).unwrap();
    vol.sync_fs().unwrap();
    let (_d2, v2) = mount(dev.image());
    let ino = lookup(&v2, "/saved");
    assert_eq!(v2.stat(ino).unwrap().size(), 12_345);
    let mut b = vec![0u8; 12_345];
    assert_eq!(v2.read(ino, 0, &mut b).unwrap(), 12_345);
    assert!(b.iter().all(|&x| x == 0x22), "the renamed file's data did not reach the disk before the rename");
}
