//! FS-1b: chmod/chown/utimensat and statfs on vfat as Linux's `fat_setattr`/`fat_statfs` answer them.
use super::rules::stick_image;
use super::util::*;
use std::sync::Arc;
use uapi::errno::*;
use vfs::abi::Time;
use vfs::inode::{Cred, Meta};
use vfs::ops::Ctx;
use vfs::setattr::{self, SetAttr};

fn set(c: &Ctx<'_>, path: &str, build: impl Fn(&Meta) -> SetAttr) -> Result<(), i64> {
    let p = c.lookup(None, path.as_bytes(), false)?;
    c.setattr(&p, build)
}

fn mode(ns: &vfs::MountNs, path: &str) -> u32 {
    ns.lookup_path(path.as_bytes(), false).unwrap().attr().mode & 0o7777
}

#[test]
fn setattr_like_linux_vfat() {
    let dir = scratch("attrs");
    let img = mkfs(&dir, 40, 1, "ATTRS");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    ns.create_file_bytes(b"/boot/f.txt", 0o644, b"x").unwrap();
    c.mkdir(None, b"/boot/d", 0o755).unwrap();
    assert_eq!(mode(&ns, "/boot/f.txt"), 0o755);
    // r/x bits FAT cannot show: dropped without an error; suid/sgid/sticky: EPERM.
    set(&c, "/boot/f.txt", |_| setattr::chmod(0o644)).unwrap();
    assert_eq!(mode(&ns, "/boot/f.txt"), 0o755);
    assert_eq!(set(&c, "/boot/f.txt", |_| setattr::chmod(0o4755)), Err(EPERM));
    // The owner's w bit is the read-only attribute, on the disk.
    set(&c, "/boot/f.txt", |_| setattr::chmod(0o555)).unwrap();
    assert_eq!(mode(&ns, "/boot/f.txt"), 0o555);
    assert_ne!(vol.peek(&[b"f.txt"]).unwrap().unwrap().attr & crate::dirent::ATTR_RO, 0);
    set(&c, "/boot/f.txt", |_| setattr::chmod(0o775)).unwrap();
    assert_eq!(mode(&ns, "/boot/f.txt"), 0o755);
    assert_eq!(vol.peek(&[b"f.txt"]).unwrap().unwrap().attr & crate::dirent::ATTR_RO, 0);
    // A directory cannot hold the attribute (no `rodir`): a mode without the owner's w is dropped.
    set(&c, "/boot/d", |_| setattr::chmod(0o555)).unwrap();
    assert_eq!(mode(&ns, "/boot/d"), 0o755);
    // Owner: only root (uid/gid 0) is accepted, unchanged values pass.
    set(&c, "/boot/f.txt", |m| setattr::chown(m, Cred::ROOT, 0, 0)).unwrap();
    assert_eq!(set(&c, "/boot/f.txt", |m| setattr::chown(m, Cred::ROOT, 1000, u32::MAX)), Err(EPERM));
    assert_eq!(set(&c, "/boot/f.txt", |m| setattr::chown(m, Cred::ROOT, u32::MAX, 5)), Err(EPERM));
    // Another user is not the owner: EPERM before vfat is asked.
    let user = Ctx { cred: Cred { uid: 1000, gid: 1000 }, ..Ctx::kernel(&ns) };
    assert_eq!(set(&user, "/boot/f.txt", |_| setattr::chmod(0o755)), Err(EPERM));
    // Times as FAT keeps them: mtime to 2 s (ctime follows), atime to the day; the entry has them.
    let t = 1_760_000_001i64;
    let a = setattr::utimes(Some([(t, 7), (t, 9)])).unwrap();
    set(&c, "/boot/f.txt", |_| a).unwrap();
    let st = ns.lookup_path(b"/boot/f.txt", false).unwrap().attr();
    assert_eq!((st.mtime, st.ctime, st.atime), (Time { sec: t - 1, nsec: 0 }, Time { sec: t - 1, nsec: 0 }, Time { sec: t - t % 86_400, nsec: 0 }));
    vol.sync(true).unwrap();
    let e = vol.peek(&[b"f.txt"]).unwrap().unwrap();
    assert_eq!(vol.clock.utc(e.wdate, e.wtime), t - 1);
    // A ctime-only change (chown to the same owner) leaves the times.
    set(&c, "/boot/f.txt", |m| setattr::chown(m, Cred::ROOT, u32::MAX, u32::MAX)).unwrap();
    assert_eq!(ns.lookup_path(b"/boot/f.txt", false).unwrap().attr().ctime, Time { sec: t - 1, nsec: 0 });
    // statfs: clusters, free clusters, FAT_LFN_LEN * NLS_MAX_CHARSET_SIZE.
    let p = ns.lookup_path(b"/boot/f.txt", false).unwrap();
    let s = vfs::statfs::of(&p.dentry.sb, false).unwrap();
    let (cb, clusters, free) = vol.statfs().unwrap();
    assert_eq!((s.ftype, s.bsize as u64, s.blocks, s.bfree, s.bavail, s.namelen, s.frsize), (0x4d44, cb, clusters, free, free, 1530, cb as i64));
    assert_clean(&dir, &dev.image());
}

#[test]
fn stick_rules_hold_for_setattr() {
    let dir = scratch("attrs-stick");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, true), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let c = Ctx::kernel(&ns);
    let now = setattr::utimes(None).unwrap();
    assert_eq!(set(&c, "/boot/EFI/BOOT/BOOTX64.EFI", |_| now), Err(EROFS));
    assert_eq!(set(&c, "/boot/EFI/BOOT/BOOTX64.EFI", |_| setattr::chmod(0o555)), Err(EROFS));
    // A mode change FAT drops writes nothing, so it passes even there (Linux answers 0 too).
    set(&c, "/boot/EFI/BOOT/BOOTX64.EFI", |_| setattr::chmod(0o644)).unwrap();
    // Attribute and times together are one entry write: refused as a whole, nothing half done.
    let before = mode(&ns, "/boot/EFI/BOOT/BOOTX64.EFI");
    let both = |_: &Meta| SetAttr { valid: setattr::chmod(0o555).valid | now.valid, mode: 0o555, ..now };
    assert_eq!(set(&c, "/boot/EFI/BOOT/BOOTX64.EFI", both), Err(EROFS));
    assert_eq!(mode(&ns, "/boot/EFI/BOOT/BOOTX64.EFI"), before);
    // The crafted entry that hides behind STICK.ID's alias stays protected; a program file may change.
    assert_eq!(set(&c, "/boot/BITSOS2/Innocent file.txt", |_| now), Err(EPERM));
    set(&c, "/boot/BITSOS2/bin/hello", |_| now).unwrap();
}

#[test]
fn fallocate_like_linux_vfat() {
    let dir = scratch("falloc");
    let img = mkfs(&dir, 40, 1, "FALLOC");
    let dev = load(&img);
    let vol = mount_plain(&dev);
    let ns = view(&vol);
    let d = ns.create_file_bytes(b"/boot/g.bin", 0o644, b"abc").unwrap();
    let i = &d.inode;
    // Mode 0 grows the file (zeros), within the size it changes nothing.
    i.fallocate(0, 0, 2).unwrap();
    assert_eq!(i.size(), 3);
    i.fallocate(0, 5000, 5000).unwrap();
    assert_eq!(i.size(), 10_000);
    let mut b = [9u8; 10];
    assert_eq!(i.read_at(9_990, &mut b).unwrap(), 10);
    assert_eq!(b, [0; 10]);
    // KEEP_SIZE promises space (ENOSPC beyond the volume) and keeps the size; other modes EOPNOTSUPP.
    let (cb, _, free) = vol.statfs().unwrap();
    i.fallocate(1, 0, 20 * cb).unwrap();
    assert_eq!(i.size(), 10_000);
    assert_eq!(i.fallocate(1, 0, (free + 50) * cb), Err(ENOSPC));
    assert_eq!(i.fallocate(3, 0, 10), Err(EOPNOTSUPP));
    i.sync(vfs::pagecache::SyncKind::Fsync).unwrap();
    vol.sync(true).unwrap();
    assert_eq!(get(&save(&dir, &dev), "/g.bin").unwrap().len(), 10_000);
    assert_clean(&dir, &dev.image());
}

/// INT-W6 (YT-2 CR): `\BITSOS2\home\` belongs to user 1000 (0700/0600), like vfat with uid=1000,dmask=077,fmask=177.
#[test]
fn stick_home_belongs_to_user_1000() {
    use vfs::abi::{O_CREAT, O_WRONLY};
    let dir = scratch("attrs-home");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, true), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let root = Ctx::kernel(&ns);
    root.mkdir(None, b"/boot/BITSOS2/Home", 0o755).unwrap();
    let st = ns.lookup_path(b"/boot/BITSOS2/Home", false).unwrap().attr();
    assert_eq!((st.uid, st.gid, st.mode & 0o7777), (1000, 1000, 0o700), "by its alias HOME, whatever the case");
    let user = Ctx { cred: Cred { uid: 1000, gid: 1000 }, ..Ctx::kernel(&ns) };
    user.mkdir(None, b"/boot/BITSOS2/Home/.local", 0o755).unwrap();
    user.open(None, b"/boot/BITSOS2/Home/.local/a", O_CREAT | O_WRONLY, 0o644).unwrap().pwrite(0, b"one").unwrap();
    user.open(None, b"/boot/BITSOS2/Home/.local/b", O_CREAT | O_WRONLY, 0o644).unwrap().pwrite(0, b"two").unwrap();
    let st = ns.lookup_path(b"/boot/BITSOS2/Home/.local/a", false).unwrap().attr();
    assert_eq!((st.uid, st.gid, st.mode & 0o7777), (1000, 1000, 0o600));
    assert_eq!(mode(&ns, "/boot/BITSOS2/Home/.local"), 0o700);
    user.rename(None, b"/boot/BITSOS2/Home/.local/b", None, b"/boot/BITSOS2/Home/.local/a", 0).unwrap();
    assert_eq!(ns.read_file("/boot/BITSOS2/Home/.local/a", 16).unwrap(), b"two");
    // Its own owner is a no-op, any other EPERM; a mode without w is the read-only attribute (0400).
    set(&user, "/boot/BITSOS2/Home/.local/a", |m| setattr::chown(m, Cred { uid: 1000, gid: 1000 }, 1000, 1000)).unwrap();
    assert_eq!(set(&root, "/boot/BITSOS2/Home/.local/a", |m| setattr::chown(m, Cred::ROOT, 0, 0)), Err(EPERM));
    set(&user, "/boot/BITSOS2/Home/.local/a", |_| setattr::chmod(0o400)).unwrap();
    assert_eq!(mode(&ns, "/boot/BITSOS2/Home/.local/a"), 0o400);
    set(&user, "/boot/BITSOS2/Home/.local/a", |_| setattr::chmod(0o644)).unwrap();
    assert_eq!(mode(&ns, "/boot/BITSOS2/Home/.local/a"), 0o600);
    user.unlink(None, b"/boot/BITSOS2/Home/.local/a").unwrap();
    // Elsewhere in \BITSOS2\ user 1000 is not the owner.
    assert_eq!(user.open(None, b"/boot/BITSOS2/x.txt", O_CREAT | O_WRONLY, 0o644).err(), Some(EACCES));
    assert_eq!(user.mkdir(None, b"/boot/BITSOS2/bin/x", 0o755).err(), Some(EACCES));
    // Only directly below \BITSOS2\: a "home" deeper down is an ordinary directory.
    root.mkdir(None, b"/boot/BITSOS2/bin/home", 0o755).unwrap();
    assert_eq!(ns.lookup_path(b"/boot/BITSOS2/bin/home", false).unwrap().attr().uid, 0);
}

/// INT-W7 (MC-push CR 4 to FS): a rename into or out of `\BITSOS2\home\` gives the moved node and its cached
/// subtree the owner and mode of the new place at once, not only after the next mount.
#[test]
fn rename_across_home_takes_the_new_owner() {
    use vfs::abi::{O_CREAT, O_WRONLY};
    let dir = scratch("attrs-home-rename");
    let dev = Arc::new(crate::MemDev::new(stick_image(&dir, false), 512));
    let vol = mount_stick(&dev);
    let ns = view(&vol);
    let root = Ctx::kernel(&ns);
    let st = |p: &str| {
        let a = ns.lookup_path(p.as_bytes(), false).unwrap().attr();
        (a.uid, a.gid, a.mode & 0o7777)
    };
    root.mkdir(None, b"/boot/BITSOS2/home", 0o755).unwrap();
    root.mkdir(None, b"/boot/BITSOS2/stage", 0o755).unwrap();
    root.mkdir(None, b"/boot/BITSOS2/stage/d", 0o755).unwrap();
    root.open(None, b"/boot/BITSOS2/stage/f", O_CREAT | O_WRONLY, 0o644).unwrap().pwrite(0, b"f").unwrap();
    root.open(None, b"/boot/BITSOS2/stage/d/g", O_CREAT | O_WRONLY, 0o644).unwrap().pwrite(0, b"g").unwrap();
    assert_eq!(st("/boot/BITSOS2/stage/f"), (0, 0, 0o755));
    root.rename(None, b"/boot/BITSOS2/stage/f", None, b"/boot/BITSOS2/home/f", 0).unwrap();
    assert_eq!(st("/boot/BITSOS2/home/f"), (1000, 1000, 0o600));
    root.rename(None, b"/boot/BITSOS2/stage/d", None, b"/boot/BITSOS2/home/d", 0).unwrap();
    assert_eq!(st("/boot/BITSOS2/home/d"), (1000, 1000, 0o700));
    assert_eq!(st("/boot/BITSOS2/home/d/g"), (1000, 1000, 0o600));
    // And back out: root's again, 0755.
    root.rename(None, b"/boot/BITSOS2/home/d", None, b"/boot/BITSOS2/stage/d", 0).unwrap();
    assert_eq!(st("/boot/BITSOS2/stage/d"), (0, 0, 0o755));
    assert_eq!(st("/boot/BITSOS2/stage/d/g"), (0, 0, 0o755));
    assert_clean(&dir, &dev.image());
}
