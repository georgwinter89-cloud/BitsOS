//! SX-3a host tests: bind mounts, remount, propagation, unprivileged copies and their locks, pivot_root,
//! umount2's flags, mountinfo, and the three syscalls through a fake `MountPerms`.

extern crate std;

use crate::abi::{O_CREAT, O_WRONLY};
use crate::cases_test::Mem;
use crate::fd::{FdTable, FsState};
use crate::inode::Cred;
use crate::mount_fs::Mounter;
use crate::mount_ns::*;
use crate::mount_ops::{MNT_DETACH, MNT_EXPIRE, Propagation};
use crate::ops::Ctx;
use crate::sys::Sys;
use crate::sys_mount::{self, *};
use crate::{Errno, SuperBlock, tmpfs};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::Cell;
use uapi::errno::*;

fn errno<T>(r: Result<T, i64>) -> i64 {
    r.err().unwrap_or(0)
}

/// A view with /a, /b, /c and a tmpfs on /a.
fn view() -> (Arc<MountNs>, Path) {
    let ns = Arc::new(MountNs::new_empty());
    let c = Ctx::kernel(&ns);
    for d in [&b"/a"[..], b"/b", b"/c", b"/d"] {
        c.mkdir(None, d, 0o755).unwrap();
    }
    let at = c.lookup(None, b"/a", true).unwrap();
    let (sb, root) = tmpfs::new_fs(0o755);
    ns.mount(&at, sb, root, false).unwrap();
    let top = c.lookup(None, b"/a", true).unwrap();
    c.mkdir(None, b"/a/sub", 0o755).unwrap();
    c.open(None, b"/a/sub/f", O_CREAT | O_WRONLY, 0o644).unwrap();
    (ns, top)
}

fn info(ns: &MountNs) -> String {
    crate::mountinfo::render(ns, &ns.root_path(), true)
}

/// mountinfo lines with the ids left out: "<root> <point> <opts and tags> - <type>".
fn shape(ns: &MountNs) -> Vec<String> {
    info(ns)
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.split(' ').collect();
            let dash = f.iter().position(|x| *x == "-").unwrap();
            let tags: Vec<&str> = f[6..dash].iter().map(|t| t.split(':').next().unwrap()).collect();
            alloc::format!("{} {} {} {} - {}", f[3], f[4], f[5], tags.join(","), f[dash + 1])
        })
        .collect()
}

#[test]
fn bind_and_remount_readonly() {
    let (ns, top) = view();
    let c = Ctx::kernel(&ns);
    let sub = c.lookup(None, b"/a/sub", true).unwrap();
    let b = c.lookup(None, b"/b", true).unwrap();
    let m = ns.bind(&b, &sub, false).unwrap();
    assert!(c.lookup(None, b"/b/f", true).is_ok(), "the bind shows the source directory");
    assert!(Arc::ptr_eq(&c.lookup(None, b"/b/f", true).unwrap().mnt, &m));
    // MS_REMOUNT|MS_BIND|MS_RDONLY: the bind alone is read-only.
    let bp = c.lookup(None, b"/b", true).unwrap();
    ns.remount_check(&bp, MNT_READONLY | MNT_RELATIME).unwrap();
    ns.remount_apply(&bp, MNT_READONLY | MNT_RELATIME, None);
    assert_eq!(errno(c.open(None, b"/b/g", O_CREAT | O_WRONLY, 0o644)), EROFS);
    assert!(c.open(None, b"/a/sub/g", O_CREAT | O_WRONLY, 0o644).is_ok(), "the source stays writable");
    let s = shape(&ns);
    assert!(s.iter().any(|l| l == "/sub /b ro,relatime  - tmpfs"), "{s:?}");
    assert!(info(&ns).lines().any(|l| l.contains(" /b ro,relatime - tmpfs tmpfs rw")), "super block stays rw: {}", info(&ns));
    // A file onto a file; a directory onto a file is ENOTDIR.
    c.open(None, b"/c/file", O_CREAT | O_WRONLY, 0o644).unwrap();
    let file = c.lookup(None, b"/c/file", true).unwrap();
    let src = c.lookup(None, b"/a/sub/f", true).unwrap();
    ns.bind(&file, &src, false).unwrap();
    assert!(c.lookup(None, b"/c/file", true).unwrap().is_mount_root());
    assert_eq!(errno(ns.bind(&file, &top, false)), ENOTDIR);
    // A recursive bind takes the mounts below; a plain one does not.
    let d = c.lookup(None, b"/d", true).unwrap();
    let root = ns.root_path();
    ns.bind(&d, &root, true).unwrap();
    assert!(c.lookup(None, b"/d/a/sub/f", true).is_ok());
    assert!(c.lookup(None, b"/d/b/f", true).is_ok());
}

#[test]
fn shared_peers_and_slaves() {
    let (ns, top) = view();
    let c = Ctx::kernel(&ns);
    ns.change_type(&top, Propagation::Shared, false).unwrap();
    let b = c.lookup(None, b"/b", true).unwrap();
    ns.bind(&b, &top, false).unwrap();
    let cdir = c.lookup(None, b"/c", true).unwrap();
    ns.bind(&cdir, &top, false).unwrap();
    let cp = c.lookup(None, b"/c", true).unwrap();
    ns.change_type(&cp, Propagation::Slave, false).unwrap();
    // A mount on the shared /a shows on its peer /b and its slave /c.
    let at = c.lookup(None, b"/a/sub", true).unwrap();
    let (sb, root) = tmpfs::new_fs(0o755);
    ns.mount(&at, sb, root, false).unwrap();
    c.open(None, b"/a/sub/new", O_CREAT | O_WRONLY, 0o644).unwrap();
    assert!(c.lookup(None, b"/b/sub/new", true).is_ok(), "peer");
    assert!(c.lookup(None, b"/c/sub/new", true).is_ok(), "slave");
    let s = shape(&ns);
    assert!(s.iter().filter(|l| l.contains(" /c ")).all(|l| l.contains("master")), "{s:?}");
    assert!(s.iter().filter(|l| l.contains(" /b ")).all(|l| l.contains("shared")), "{s:?}");
    // A mount on the slave stays there.
    let (sb2, r2) = tmpfs::new_fs(0o755);
    let c2 = c.lookup(None, b"/c/sub", true).unwrap();
    ns.mount(&c2, sb2, r2, false).unwrap();
    c.open(None, b"/c/sub/only", O_CREAT | O_WRONLY, 0o644).unwrap();
    assert_eq!(errno(c.lookup(None, b"/a/sub/only", true)), ENOENT);
    // Unmounting on /a takes the peer's copy too.
    let a_sub = c.lookup(None, b"/a/sub", true).unwrap();
    ns.umount_user(a_sub, 0).unwrap();
    assert_eq!(errno(c.lookup(None, b"/b/sub/new", true)), ENOENT);
    // Private: /a stops propagating.
    ns.change_type(&top, Propagation::Private, false).unwrap();
    let (sb3, r3) = tmpfs::new_fs(0o755);
    ns.mount(&c.lookup(None, b"/a/sub", true).unwrap(), sb3, r3, false).unwrap();
    c.open(None, b"/a/sub/p", O_CREAT | O_WRONLY, 0o644).unwrap();
    assert_eq!(errno(c.lookup(None, b"/b/sub/p", true)), ENOENT);
}

#[test]
fn unprivileged_copy_is_locked() {
    let (ns, top) = view();
    ns.change_type(&top, Propagation::Shared, false).unwrap();
    let ro_at = Ctx::kernel(&ns).lookup(None, b"/c", true).unwrap();
    let (sb, root) = tmpfs::new_fs(0o755);
    let ro = ns.mount(&ro_at, sb, root, true).unwrap();
    ro.set_flags(MNT_NOSUID | MNT_RELATIME);
    let copy = Arc::new(ns.copy_for(7));
    let c = Ctx::kernel(&copy);
    let a = c.lookup(None, b"/a", true).unwrap();
    assert!(a.mnt.flags() & MNT_LOCKED != 0 && a.mnt.propagation().0 == 0 && a.mnt.propagation().1 != 0, "a slave, locked");
    assert_eq!(errno(copy.umount_user(a.clone(), 0)), EINVAL);
    assert_eq!(errno(copy.umount_user(a.clone(), MNT_DETACH)), EINVAL);
    // The locked read-only and nosuid flags stay.
    let cm = c.lookup(None, b"/c", true).unwrap();
    assert_eq!(errno(copy.remount_check(&cm, MNT_RELATIME | MNT_NOSUID)), EPERM);
    assert_eq!(errno(copy.remount_check(&cm, MNT_READONLY | MNT_RELATIME)), EPERM);
    assert_eq!(errno(copy.remount_check(&cm, MNT_READONLY | MNT_NOSUID | MNT_NOATIME)), EPERM, "atime is locked too");
    copy.remount_check(&cm, MNT_READONLY | MNT_NOSUID | MNT_NODEV | MNT_RELATIME).unwrap();
    // A plain bind of the root would uncover what the locked mounts hide; a recursive one is fine.
    let d = c.lookup(None, b"/d", true).unwrap();
    let root = copy.root_path();
    assert_eq!(errno(copy.bind(&d, &root, false)), EINVAL);
    copy.bind(&d, &root, true).unwrap();
    // A mount in the parent view reaches the copy's slave (locked below its top).
    let (sb2, r2) = tmpfs::new_fs(0o755);
    ns.mount(&Ctx::kernel(&ns).lookup(None, b"/a/sub", true).unwrap(), sb2, r2, false).unwrap();
    Ctx::kernel(&ns).open(None, b"/a/sub/late", O_CREAT | O_WRONLY, 0o644).unwrap();
    assert!(c.lookup(None, b"/a/sub/late", true).is_ok());
    let got = c.lookup(None, b"/a/sub", true).unwrap();
    assert_eq!(got.mnt.flags() & MNT_LOCKED, 0, "a propagated copy's top can be unmounted");
    // ... and nothing made in the copy reaches back.
    let (sb3, r3) = tmpfs::new_fs(0o755);
    copy.add_mount(&c.lookup(None, b"/b", true).unwrap(), sb3, r3, MNT_RELATIME).unwrap();
    assert!(Ctx::kernel(&ns).lookup(None, b"/b", true).unwrap().mnt.ns_id() == ns.id);
    assert!(!Ctx::kernel(&ns).lookup(None, b"/b", true).unwrap().is_mount_root());
}

#[test]
fn pivot_root_both_forms() {
    let (ns, _) = view();
    let c = Ctx::kernel(&ns);
    // bwrap: a tmpfs base, newroot bound onto itself, pivot_root(base, base/oldroot).
    let (sb, root) = tmpfs::new_fs(0o755);
    let base = ns.mount(&c.lookup(None, b"/d", true).unwrap(), sb, root, false).unwrap();
    for d in [&b"/d/newroot"[..], b"/d/oldroot"] {
        c.mkdir(None, d, 0o755).unwrap();
    }
    let nr = c.lookup(None, b"/d/newroot", true).unwrap();
    ns.bind(&nr, &nr, true).unwrap();
    let old_root = ns.root_path();
    let new = c.lookup(None, b"/d", true).unwrap();
    // Errors first: the root itself as new root is EBUSY; a shared place EINVAL.
    assert_eq!(errno(ns.pivot_root(&old_root, &old_root, &c.lookup(None, b"/d/oldroot", true).unwrap())), EBUSY);
    ns.pivot_root(&old_root, &new, &c.lookup(None, b"/d/oldroot", true).unwrap()).unwrap();
    assert!(Arc::ptr_eq(&ns.root_mount(), &base), "the base is the view's root now");
    let c = Ctx::kernel(&ns);
    assert!(c.lookup(None, b"/oldroot/a/sub/f", true).is_ok(), "the old root sits on oldroot");
    // pivot_root(".", ".") from newroot: the base goes on top of newroot, then the lazy unmount drops it.
    let n2 = c.lookup(None, b"/newroot", true).unwrap();
    let r = ns.root_path();
    ns.pivot_root(&r, &n2, &n2).unwrap();
    let top = ns.cross_down(n2.clone());
    assert!(Arc::ptr_eq(&top.mnt, &base), "the old root is on top of the new one");
    ns.umount_user(top, MNT_DETACH).unwrap();
    let r = ns.root_path();
    assert!(Arc::ptr_eq(&r.mnt, &n2.mnt));
    assert!(ns.mounts().iter().all(|m| !Arc::ptr_eq(m, &base)), "the base left the view");
    assert_eq!(info(&ns).lines().count(), ns.mounts().len());
}

#[test]
fn umount_busy_expire_and_lazy() {
    let (ns, top) = view();
    let held = top.clone();
    assert_eq!(errno(ns.umount_user(top.clone(), 0)), EBUSY, "a reference is a user");
    assert_eq!(errno(ns.umount_user(top.clone(), MNT_EXPIRE | MNT_DETACH)), EINVAL);
    assert_eq!(errno(ns.umount_user(top.clone(), MNT_EXPIRE)), EAGAIN);
    ns.umount_user(top, MNT_DETACH).unwrap();
    // The held place still works (a lazy unmount keeps its users' places).
    assert!(held.dentry.child(b"sub").is_some());
    assert_eq!(held.mnt.ns_id(), 0);
    let c = Ctx::kernel(&ns);
    assert_eq!(errno(c.lookup(None, b"/a/sub", true)), ENOENT);
    let not_root = c.lookup(None, b"/b", true).unwrap();
    assert_eq!(errno(ns.umount_user(not_root, 0)), EINVAL);
}

#[test]
fn mountinfo_from_a_root() {
    let (ns, _) = view();
    let c = Ctx::kernel(&ns);
    let sub = c.lookup(None, b"/a/sub", true).unwrap();
    ns.bind(&c.lookup(None, b"/b", true).unwrap(), &sub, false).unwrap();
    let text = info(&ns);
    assert_eq!(text.lines().count(), 3, "{text}");
    assert!(text.lines().next().unwrap().contains(" / / rw"), "{text}");
    // Seen from /a: the root mount is outside, /a is "/", the bind on /b is outside.
    let from_a = crate::mountinfo::render(&ns, &c.lookup(None, b"/a", true).unwrap(), true);
    assert_eq!(from_a.lines().count(), 1, "{from_a}");
    assert!(from_a.contains(" / / rw,relatime - tmpfs"), "{from_a}");
    let mounts = crate::mountinfo::render(&ns, &ns.root_path(), false);
    assert!(mounts.lines().any(|l| l == "tmpfs /b tmpfs rw,relatime,mode=755,inode64 0 0"), "{mounts}");
}

#[test]
fn fs_state_follows_the_copy() {
    let (ns, top) = view();
    let fs = FsState::new();
    fs.set_cwd(Path { mnt: top.mnt.clone(), dentry: top.dentry.child(b"sub").unwrap() });
    fs.enter(&ns);
    let copy = ns.copy_for(3);
    fs.enter(&copy);
    let cwd = fs.cwd().unwrap();
    assert_eq!(cwd.mnt.ns_id(), copy.id, "the cwd moved onto the copy");
    assert_eq!(copy.path_of(&copy.root_path(), &cwd).unwrap(), b"/a/sub");
    // Back with the source view (an old caller): it stays in the copy.
    fs.enter(&ns);
    assert_eq!(fs.cwd().unwrap().mnt.ns_id(), copy.id);
}

/// Everything allowed, as a namespace's root; the decisions are counted.
struct Fake {
    admin: bool,
    pivots: Cell<usize>,
}

impl MountPerms for Fake {
    fn may_mount(&self) -> bool {
        self.admin
    }
    fn capable_init(&self) -> bool {
        false
    }
    fn may_mount_fs(&self, _: &str) -> bool {
        self.admin
    }
    fn fs_owner(&self, _: &str) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        None
    }
    fn admin_over(&self, _: &SuperBlock) -> bool {
        false
    }
    fn ns_owned_by_init(&self) -> bool {
        false
    }
    fn with_mounter(&self, f: &mut dyn FnMut(&Mounter<'_>) -> Result<(), Errno>) -> Result<(), Errno> {
        let map = |n: u32| (n < 1).then_some(n + 1000);
        f(&Mounter { fsuid: 1000, fsgid: 1000, uid_in: &map, gid_in: &map, init_ns: false, ns_root: Some((1000, 1000)) })
    }
    fn ptmx(&self) -> Option<Arc<dyn crate::File>> {
        None
    }
    fn ram_bytes(&self) -> u64 {
        1 << 30
    }
    fn chroot_fs_refs(&self, _: &Path, _: &Path) {
        self.pivots.set(self.pivots.get() + 1);
    }
}

const USER: Cred = Cred { uid: 1000, gid: 1000 };

#[test]
fn syscalls_in_linux_order() {
    let (ns, _) = view();
    let mem = Mem::new();
    let fds = FdTable::new();
    let fs = FsState::new();
    let s = Sys { mem: &mem, fds: &fds, fs: &fs, ns: &ns, cred: Cred::ROOT, subject: policy::Subject::KERNEL };
    let p = Fake { admin: true, pivots: Cell::new(0) };
    let st = |x: &[u8]| mem.cstr(x);
    let call = |f: fn(&Sys<'_>, crate::sys::Args, &dyn MountPerms) -> i64, a: [u64; 6]| f(&s, a, &p);
    // Type unknown, no type, bad options, a bad pointer, a missing directory.
    assert_eq!(call(sys_mount::mount, [st(b"x"), st(b"/b"), st(b"ext9"), 0, 0, 0]), -ENODEV);
    assert_eq!(call(sys_mount::mount, [st(b"x"), st(b"/b"), 0, 0, 0, 0]), -EINVAL);
    assert_eq!(call(sys_mount::mount, [st(b"x"), st(b"/b"), st(b"tmpfs"), 0, st(b"bogus=1"), 0]), -EINVAL);
    assert_eq!(call(sys_mount::mount, [st(b"x"), st(b"/b"), st(b"tmpfs"), 0, st(b"nosuid"), 0]), -EINVAL);
    assert_eq!(call(sys_mount::mount, [st(b"x"), st(b"/b"), st(b"tmpfs"), 0, st(b"uid=5"), 0]), -EINVAL, "an unmapped uid");
    assert_eq!(call(sys_mount::mount, [st(b"x"), 0x10, st(b"tmpfs"), 0, 0, 0]), -EFAULT);
    assert_eq!(call(sys_mount::mount, [st(b"x"), st(b"/nope"), st(b"tmpfs"), 0, 0, 0]), -ENOENT);
    assert_eq!(call(sys_mount::mount, [st(b"x"), st(b"/b"), st(b"tmpfs"), MS_NOUSER, 0, 0]), -EINVAL);
    let flags = MS_NOSUID | MS_NODEV | 0xC0ED_0000;
    assert_eq!(call(sys_mount::mount, [st(b"tmpfs"), st(b"/b"), st(b"tmpfs"), flags, st(b"size=1m,mode=0755"), 0]), 0);
    let text = info(&ns);
    assert!(text.lines().any(|l| l.ends_with(" /b rw,nosuid,nodev,relatime - tmpfs tmpfs rw,size=1024k,mode=755,uid=1000,gid=1000,inode64")), "{text}");
    // Propagation flags: one at a time, on a mount root.
    assert_eq!(call(sys_mount::mount, [0, st(b"/"), 0, MS_SLAVE | MS_SHARED, 0, 0]), -EINVAL);
    assert_eq!(call(sys_mount::mount, [0, st(b"/c"), 0, MS_PRIVATE, 0, 0]), -EINVAL);
    assert_eq!(call(sys_mount::mount, [0, st(b"/"), 0, MS_SLAVE | MS_REC, 0, 0]), 0);
    // Binds need a source; remount of a bind read-only.
    assert_eq!(call(sys_mount::mount, [st(b""), st(b"/c"), 0, MS_BIND, 0, 0]), -EINVAL);
    assert_eq!(call(sys_mount::mount, [st(b"/a/sub"), st(b"/c"), 0, MS_BIND | MS_REC, 0, 0]), 0);
    assert_eq!(call(sys_mount::mount, [0, st(b"/c"), 0, MS_REMOUNT | MS_BIND | MS_RDONLY, 0, 0]), 0);
    assert!(Ctx::kernel(&ns).lookup(None, b"/c", true).unwrap().mnt.is_readonly());
    // An sb remount needs CAP_SYS_ADMIN over the file system (the fake has none).
    assert_eq!(call(sys_mount::mount, [0, st(b"/b"), 0, MS_REMOUNT | MS_RDONLY, 0, 0]), -EPERM);
    // MS_MOVE.
    assert_eq!(call(sys_mount::mount, [st(b"/c"), st(b"/d"), 0, MS_MOVE, 0, 0]), 0);
    assert!(Ctx::kernel(&ns).lookup(None, b"/d/f", true).is_ok());
    // umount2: flags, then lookup, then the tree.
    assert_eq!(call(sys_mount::umount2, [st(b"/d"), 0x10, 0, 0, 0, 0]), -EINVAL);
    assert_eq!(call(sys_mount::umount2, [st(b"/c"), 0, 0, 0, 0, 0]), -EINVAL);
    assert_eq!(call(sys_mount::umount2, [st(b"/d"), 0, 0, 0, 0, 0]), 0);
    // pivot_root: a file is ENOTDIR; then the view's root changes and the roots are re-pointed.
    Ctx::kernel(&ns).open(None, b"/b/file", O_CREAT | O_WRONLY, 0o644).unwrap();
    assert_eq!(call(sys_mount::pivot_root, [st(b"/b/file"), st(b"/b"), 0, 0, 0, 0]), -ENOTDIR);
    Ctx::kernel(&ns).mkdir(None, b"/b/old", 0o755).unwrap();
    assert_eq!(call(sys_mount::pivot_root, [st(b"/b"), st(b"/b/old"), 0, 0, 0, 0]), 0);
    assert_eq!(p.pivots.get(), 1);
    // Without the capability: EPERM after the lookup (mount), before it (pivot_root).
    let q = Fake { admin: false, pivots: Cell::new(0) };
    let s2 = Sys { mem: &mem, fds: &fds, fs: &fs, ns: &ns, cred: USER, subject: policy::Subject::KERNEL };
    assert_eq!(sys_mount::mount(&s2, [0, st(b"/nope"), st(b"tmpfs"), 0, 0, 0], &q), -ENOENT);
    assert_eq!(sys_mount::mount(&s2, [0, st(b"/"), st(b"tmpfs"), 0, 0, 0], &q), -EPERM);
    assert_eq!(sys_mount::pivot_root(&s2, [st(b"/nope"), st(b"/"), 0, 0, 0, 0], &q), -EPERM);
    assert_eq!(sys_mount::umount2(&s2, [st(b"/old"), 0, 0, 0, 0, 0], &q), -EPERM);
}

#[test]
fn overlay_merges_read_only() {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    for d in [&b"/l1"[..], b"/l2", b"/l1/d", b"/l2/d", b"/l2/gone", b"/m"] {
        c.mkdir(None, d, 0o755).unwrap();
    }
    ns.create_file("/l1/a", 0o644, b"upper".to_vec()).unwrap();
    ns.create_file("/l2/a", 0o644, b"lower".to_vec()).unwrap();
    ns.create_file("/l2/d/y", 0o644, b"y".to_vec()).unwrap();
    ns.create_file("/l1/d/x", 0o644, b"x".to_vec()).unwrap();
    // A whiteout in the upper layer hides the lower directory.
    c.mknod(None, b"/l1/gone", uapi::S_IFCHR, 0).unwrap();
    let layers = alloc::vec![c.lookup(None, b"/l1", true).unwrap(), c.lookup(None, b"/l2", true).unwrap()];
    let (sb, root) = crate::overlay::new(layers, ",lowerdir=/l1:/l2");
    ns.add_mount(&c.lookup(None, b"/m", true).unwrap(), sb, root, MNT_RELATIME).unwrap();
    let names: Vec<Vec<u8>> = ns.list_dir("/m").unwrap().into_iter().map(|(n, _)| n).collect();
    assert!(names.contains(&b"a".to_vec()) && names.contains(&b"d".to_vec()) && !names.contains(&b"gone".to_vec()), "{names:?}");
    let mut buf = [0u8; 16];
    let f = ns.open("/m/a").unwrap();
    let n = f.read_at(0, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"upper");
    assert!(c.lookup(None, b"/m/d/x", true).is_ok() && c.lookup(None, b"/m/d/y", true).is_ok(), "merged directory");
    assert_eq!(errno(c.open(None, b"/m/new", O_CREAT | O_WRONLY, 0o644)), EROFS);
    assert_eq!(errno(c.unlink(None, b"/m/a")), EROFS);
    let o = mount_fs_opts(b"lowerdir=/l1");
    assert_eq!(errno(crate::overlay::parse(&o)), EINVAL, "one layer without an upper one");
    let o = mount_fs_opts(b"lowerdir=/l1:/l2,upperdir=/u,workdir=/w");
    assert_eq!(errno(crate::overlay::parse(&o)), EINVAL, "no upper layer yet");
}

fn mount_fs_opts(s: &[u8]) -> crate::mount_fs::Opts {
    crate::mount_fs::split(s).unwrap()
}

#[test]
fn refused_remount_changes_nothing() {
    let (ns, _) = view();
    let mem = Mem::new();
    let fds = FdTable::new();
    let fs = FsState::new();
    let s = Sys { mem: &mem, fds: &fds, fs: &fs, ns: &ns, cred: Cred::ROOT, subject: policy::Subject::KERNEL };
    let p = Fake { admin: true, pivots: Cell::new(0) };
    let st = |x: &[u8]| mem.cstr(x);
    assert_eq!(sys_mount::mount(&s, [st(b"tmpfs"), st(b"/b"), st(b"tmpfs"), 0, st(b"size=1m"), 0], &p), 0);
    // do_remount: a bad option is EINVAL, the missing capability over the super block EPERM, and neither
    // touches the size (the fake is no admin over any super block).
    assert_eq!(sys_mount::mount(&s, [0, st(b"/b"), 0, MS_REMOUNT, st(b"size=bad"), 0], &p), -EINVAL);
    assert_eq!(sys_mount::mount(&s, [0, st(b"/b"), 0, MS_REMOUNT, st(b"size=4k"), 0], &p), -EPERM);
    let text = info(&ns);
    assert!(text.lines().any(|l| l.contains(" /b ") && l.contains(",size=1024k")), "{text}");
    let b = Ctx::kernel(&ns).lookup(None, b"/b", true).unwrap();
    assert_eq!(b.mnt.sb.pool.size(), Some(256), "the pool keeps its 1 MiB");
}

#[test]
fn hostile_tmpfs_options() {
    let p = Fake { admin: true, pivots: Cell::new(0) };
    let parse = |s: &[u8]| {
        let o = mount_fs_opts(s);
        let mut r = Err(0);
        let _ = p.with_mounter(&mut |who| {
            r = crate::mount_fs::tmpfs_opts(&o, who, 1 << 30).map(|t| (t.size_pages, t.nr_inodes));
            Ok(())
        });
        r
    };
    assert_eq!(parse(b"size=18446744073709551615%"), Err(EINVAL), "a percentage past u64");
    assert_eq!(parse(b"size=50%"), Ok((Some((1 << 29) / 4096), None)));
    assert_eq!(parse(b"nr_blocks=9223372036854775808"), Err(EINVAL), "nr_blocks above LONG_MAX");
    assert_eq!(parse(b"nr_blocks=9223372036854775807"), Ok((Some(i64::MAX as u64), None)));
    assert_eq!(parse(b"nr_inodes=18446744073709551615"), Err(EINVAL), "nr_inodes above ULONG_MAX / BOGO_INODE_SIZE");
    assert_eq!(parse(b"size=16e"), Err(EINVAL), "a shift past u64");
    // The largest nr_blocks shows as Linux's wrapped K() and does not panic.
    let o = mount_fs_opts(b"nr_blocks=9223372036854775807");
    let _ = p.with_mounter(&mut |who| {
        let t = crate::mount_fs::tmpfs_opts(&o, who, 1 << 30)?;
        assert!(crate::mount_fs::tmpfs_show(&t).starts_with(",size="));
        Ok(())
    });
    // mpol on one memory node (mpol_parse_str).
    for ok in [&b"mpol=default"[..], b"mpol=prefer", b"mpol=prefer:0", b"mpol=bind:0", b"mpol=bind=static:0", b"mpol=interleave", b"mpol=interleave:0-0", b"mpol=local", b"mpol=prefer (many):0"] {
        assert!(parse(ok).is_ok(), "{}", String::from_utf8_lossy(ok));
    }
    for bad in [&b"mpol=bind"[..], b"mpol=bind:1", b"mpol=local:0", b"mpol=default=static", b"mpol=interleave:", b"mpol=prefer:0,0", b"mpol=bind=other:0", b"mpol=weird", b"mpol"] {
        assert_eq!(parse(bad), Err(EINVAL), "{}", String::from_utf8_lossy(bad));
    }
}

/// SX-3b: a proc mount is a new instance (`proc_get_tree` = `get_tree_nodev`) made from the registries: the
/// /proc files and directories below /proc (not /sys's), and a source on /proc itself in its form for the
/// mounter's pid namespace (`DirSource::for_pid_ns`). Two instances on one place never meet EBUSY.
#[test]
fn proc_instance_per_mount() {
    use crate::proc_instance::instance_from;
    use crate::synth::{DirSource, SynthEntry};
    use crate::{ProcDirEntry, ProcFileEntry};
    struct Ns(u32);
    impl DirSource for Ns {
        fn entries(&self) -> Result<Vec<SynthEntry>, Errno> {
            Ok(alloc::vec![SynthEntry::link("self", &alloc::format!("{}", self.0))])
        }
        fn for_pid_ns(&self, ns: &Arc<dyn core::any::Any + Send + Sync>) -> Option<Arc<dyn DirSource>> {
            Some(Arc::new(Ns(*ns.clone().downcast::<u32>().ok()?)))
        }
    }
    fn boot() -> Arc<dyn DirSource> {
        Arc::new(Ns(1))
    }
    fn text() -> String {
        String::from("x\n")
    }
    let files = [ProcFileEntry { path: "/proc/sys/kernel/sx", render: text, mode: 0o444 }, ProcFileEntry { path: "/sys/sx", render: text, mode: 0o444 }];
    let dirs = [ProcDirEntry { path: "/proc", make: boot }];
    let (ns, _) = view();
    let c = Ctx::kernel(&ns);
    for (k, key) in [7u32, 9].into_iter().enumerate() {
        let (sb, root) = instance_from(Arc::new(key), &files, &dirs).unwrap();
        assert_eq!(sb.fstype, "proc");
        let at = c.lookup(None, b"/b", true).unwrap();
        ns.add_mount(&at, sb, root, 0).unwrap_or_else(|e| panic!("mount {k}: errno {e}"));
        assert_eq!(ns.read_link("/b/self").unwrap(), alloc::format!("{key}").into_bytes(), "the instance's own pid namespace");
        assert_eq!(ns.read_file("/b/sys/kernel/sx", 16).unwrap(), b"x\n");
        assert_eq!(c.lookup(None, b"/b/sx", true).map(|_| ()), Err(ENOENT), "a /sys file is not in proc");
    }
}

/// SX-3c: `d_path` of a place the reader's root does not reach is its path up to the top of its own mount
/// chain (linux-6.6/fs/d_path.c `__prepend_path`): pressure-vessel's /proc/self/fd links of files opened before
/// the container showed "/sub/f" (the path inside the tmpfs) instead of "/a/sub/f".
#[test]
fn d_path_of_unreachable_places() {
    let (ns, _) = view();
    let c = Ctx::kernel(&ns);
    let f = c.lookup(None, b"/a/sub/f", true).unwrap();
    assert_eq!(ns.d_path(&ns.root_path(), &f), b"/a/sub/f".to_vec());
    // Another view (a copy, as unshare(CLONE_NEWNS) makes it): the original file is unreachable from its root.
    let other = Arc::new(ns.copy_view());
    assert_eq!(other.d_path(&other.root_path(), &f), b"/a/sub/f".to_vec());
    // A root inside the tmpfs (a chroot to /b, a sibling): still the whole path.
    let b = c.lookup(None, b"/b", true).unwrap();
    assert_eq!(ns.d_path(&b, &f), b"/a/sub/f".to_vec());
    // A bind of a subdirectory: from its mount root up to its mountpoint, not to the file system's root.
    ns.bind(&c.lookup(None, b"/c", true).unwrap(), &c.lookup(None, b"/a/sub", true).unwrap(), false).unwrap();
    let via_c = c.lookup(None, b"/c/f", true).unwrap();
    let other = Arc::new(ns.copy_view());
    assert_eq!(other.d_path(&other.root_path(), &via_c), b"/c/f".to_vec());
    // A detached tree: from its own root.
    let top = c.lookup(None, b"/a", true).unwrap();
    let held = c.lookup(None, b"/a/sub/f", true).unwrap();
    drop(top);
    ns.umount_user(c.lookup(None, b"/a", true).unwrap(), MNT_DETACH).unwrap();
    assert_eq!(ns.d_path(&ns.root_path(), &held), b"/sub/f".to_vec());
}

/// getcwd of the places d_path reaches only from the top of their mount chain: "(unreachable)" in front, the
/// buffer must hold that string and its NUL (ERANGE one byte short), a removed cwd is ENOENT
/// (linux-6.6/fs/d_path.c:415-445).
#[test]
fn getcwd_of_unreachable_places() {
    let (ns, _) = view();
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/a/sub/x", 0o755).unwrap();
    // A bind inside a bind: /c shows /a/sub, /d shows /c/x; the cwd is reached through both.
    ns.bind(&c.lookup(None, b"/c", true).unwrap(), &c.lookup(None, b"/a/sub", true).unwrap(), false).unwrap();
    ns.bind(&c.lookup(None, b"/d", true).unwrap(), &c.lookup(None, b"/c/x", true).unwrap(), false).unwrap();
    let mem = Mem::new();
    let fds = FdTable::new();
    let fs = FsState::new();
    let other = Arc::new(ns.copy_view());
    let s = Sys { mem: &mem, fds: &fds, fs: &fs, ns: &other, cred: Cred::ROOT, subject: policy::Subject::KERNEL };
    let cwd = |len: usize| {
        let b = mem.alloc_zero(len);
        let r = crate::sys::getcwd(&s, [b, len as u64, 0, 0, 0, 0]);
        (r, if r > 0 { mem.get(b, r as usize - 1) } else { Vec::new() })
    };
    // The cwd is a place of the first view: unreachable from the copy's root, the path is the first view's.
    fs.set_cwd(c.lookup(None, b"/d", true).unwrap());
    let want = b"(unreachable)/d".to_vec();
    assert_eq!(cwd(64), (want.len() as i64 + 1, want.clone()));
    assert_eq!(cwd(want.len()).0, -ERANGE, "no room for the NUL");
    assert_eq!(cwd(want.len() + 1).0, want.len() as i64 + 1);
    // The same place through the copy's own mounts is reachable.
    fs.set_cwd(other.lookup_path(b"/d", true).unwrap());
    assert_eq!(cwd(64).1, b"/d".to_vec());
    // A detached tree (the copy's /a lazily unmounted while the cwd is inside it): from its own root.
    fs.set_cwd(other.lookup_path(b"/a/sub/x", true).unwrap());
    other.umount_user(other.lookup_path(b"/a", true).unwrap(), MNT_DETACH).unwrap();
    assert_eq!(cwd(64).1, b"(unreachable)/sub/x".to_vec());
    assert_eq!(cwd(b"(unreachable)/sub/x".len()).0, -ERANGE);
    // A removed cwd: ENOENT, whatever the buffer.
    let held = c.lookup(None, b"/a/sub/x", true).unwrap();
    c.rmdir(None, b"/a/sub/x").unwrap();
    fs.set_cwd(held);
    assert_eq!(cwd(64).0, -ENOENT);
}

/// d_path of a removed file that is also unreachable: the top-of-chain path and " (deleted)".
#[test]
fn d_path_of_a_removed_unreachable_file() {
    let (ns, _) = view();
    let c = Ctx::kernel(&ns);
    let f = c.lookup(None, b"/a/sub/f", true).unwrap();
    c.unlink(None, b"/a/sub/f").unwrap();
    let other = Arc::new(ns.copy_view());
    assert_eq!(other.d_path(&other.root_path(), &f), b"/a/sub/f (deleted)".to_vec());
    assert_eq!(ns.d_path(&ns.root_path(), &f), b"/a/sub/f (deleted)".to_vec());
}

/// SX-3c: the hard links pressure-vessel's --copy-runtime makes, as tmpfs answers them in Linux's order
/// (linux-6.6/fs/namei.c do_linkat, may_linkat, vfs_link): the second name keeps the data when the first goes,
/// a bind of the same file system is another mount (EXDEV), a read-only target is EROFS before EXDEV,
/// fs.protected_hardlinks refuses what a user could not open read-write, AT_EMPTY_PATH without
/// CAP_DAC_READ_SEARCH is ENOENT even with a name.
#[test]
fn hard_links_on_tmpfs() {
    use crate::abi::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_FOLLOW, O_RDWR};
    let (ns, _) = view();
    let mem = Mem::new();
    let fds = FdTable::new();
    let fs = FsState::new();
    let s = Sys { mem: &mem, fds: &fds, fs: &fs, ns: &ns, cred: Cred::ROOT, subject: policy::Subject::KERNEL };
    let p = Fake { admin: true, pivots: Cell::new(0) };
    let st = |x: &[u8]| mem.cstr(x);
    let c = Ctx::kernel(&ns);
    let nlink = |path: &[u8]| c.lookup(None, path, false).unwrap().dentry.inode.meta.nlink();
    let link = |s: &Sys<'_>, a: &[u8], b: &[u8]| crate::sys::link(s, [st(a), st(b), 0, 0, 0, 0]);
    let fd = crate::sys::openat(&s, [AT_FDCWD as u64, st(b"/a/sub/f"), O_RDWR, 0, 0, 0]);
    assert!(fd >= 0);
    assert_eq!(crate::sys::write(&s, [fd as u64, mem.alloc(b"data"), 4, 0, 0, 0]), 4);
    assert_eq!(link(&s, b"/a/sub/f", b"/a/sub/g"), 0);
    assert_eq!((nlink(b"/a/sub/f"), nlink(b"/a/sub/g")), (2, 2));
    assert_eq!(link(&s, b"/a/sub/f", b"/a/sub/g"), -EEXIST);
    assert_eq!(link(&s, b"/a/sub", b"/a/dl"), -EPERM, "a directory");
    assert_eq!(link(&s, b"/a/sub/f", b"/a/new/"), -ENOENT, "a trailing slash");
    assert_eq!(crate::sys::unlink(&s, [st(b"/a/sub/f"), 0, 0, 0, 0, 0]), 0);
    assert_eq!(nlink(b"/a/sub/g"), 1);
    let g = crate::sys::openat(&s, [AT_FDCWD as u64, st(b"/a/sub/g"), 0, 0, 0, 0]);
    let buf = mem.alloc_zero(8);
    assert_eq!(crate::sys::read(&s, [g as u64, buf, 8, 0, 0, 0]), 4);
    assert_eq!(mem.get(buf, 4), b"data");
    // A bind of /a/sub on /c: the same file system, another mount.
    assert_eq!(sys_mount::mount(&s, [st(b"/a/sub"), st(b"/c"), 0, MS_BIND, 0, 0], &p), 0);
    assert_eq!(link(&s, b"/a/sub/g", b"/c/h"), -EXDEV);
    assert_eq!(link(&s, b"/c/g", b"/c/h"), 0);
    assert_eq!(nlink(b"/a/sub/h"), 2, "the bind's new name is the file system's");
    // Another tmpfs: EXDEV; a read-only bind: EROFS, also from another mount, EEXIST before both.
    assert_eq!(sys_mount::mount(&s, [st(b"sx"), st(b"/b"), st(b"tmpfs"), 0, 0, 0], &p), 0);
    assert_eq!(link(&s, b"/a/sub/g", b"/b/g"), -EXDEV);
    assert_eq!(sys_mount::mount(&s, [0, st(b"/c"), 0, MS_REMOUNT | MS_BIND | MS_RDONLY, 0, 0], &p), 0);
    assert_eq!(link(&s, b"/c/g", b"/c/i"), -EROFS);
    assert_eq!(link(&s, b"/a/sub/g", b"/c/i"), -EROFS, "filename_create's EROFS comes before EXDEV");
    assert_eq!(link(&s, b"/a/sub/g", b"/c/g"), -EEXIST);
    // AT_EMPTY_PATH: ENOENT without the capability, even with a name; root links the fd's file.
    let user = Sys { cred: USER, ..s };
    let la = |s: &Sys<'_>, d: u64, a: &[u8], b: &[u8], fl: u64| crate::sys::linkat(s, [d, st(a), AT_FDCWD as u64, st(b), fl, 0]);
    assert_eq!(la(&user, AT_FDCWD as u64, b"/a/sub/g", b"/a/sub/j", AT_EMPTY_PATH), -ENOENT);
    assert_eq!(la(&s, g as u64, b"", b"/a/sub/j", AT_EMPTY_PATH), 0);
    assert_eq!(la(&s, AT_FDCWD as u64, b"/a/sub/g", b"/a/sub/k", 0x2000), -EINVAL);
    assert_eq!(la(&s, AT_FDCWD as u64, b"/a/sub/g", b"/a/sub/k", AT_SYMLINK_FOLLOW), 0);
    assert_eq!(nlink(b"/a/sub/g"), 4);
    // An unlinked file has no name to give (not O_TMPFILE): ENOENT.
    for n in [&b"/a/sub/g"[..], b"/a/sub/h", b"/a/sub/j", b"/a/sub/k"] {
        assert_eq!(crate::sys::unlink(&s, [st(n), 0, 0, 0, 0, 0]), 0);
    }
    assert_eq!(la(&s, g as u64, b"", b"/a/sub/l", AT_EMPTY_PATH), -ENOENT);
    // fs.protected_hardlinks: a user links its own files and root's files it may read and write, nothing else.
    let mk = |path: &[u8], mode: u32, uid: u32| {
        c.open(None, path, O_CREAT | O_WRONLY, mode).unwrap();
        let m = &c.lookup(None, path, false).unwrap().dentry.inode.meta;
        m.set_owner(uid, uid);
        m.set_perm(mode);
    };
    c.mkdir(None, b"/a/u", 0o777).unwrap();
    mk(b"/a/u/root_ro", 0o644, 0);
    mk(b"/a/u/root_rw", 0o666, 0);
    mk(b"/a/u/root_suid", 0o4666, 0);
    mk(b"/a/u/root_sgid_x", 0o2676, 0);
    mk(b"/a/u/root_sgid", 0o2666, 0);
    mk(b"/a/u/mine", 0o000, 1000);
    for (src, want) in [
        (&b"/a/u/root_ro"[..], -EPERM),
        (b"/a/u/root_rw", 0),
        (b"/a/u/root_suid", -EPERM),
        (b"/a/u/root_sgid_x", -EPERM),
        (b"/a/u/root_sgid", 0),
        (b"/a/u/mine", 0),
    ] {
        let mut to = src.to_vec();
        to.extend_from_slice(b".l");
        assert_eq!(link(&user, src, &to), want, "{}", String::from_utf8_lossy(src));
    }
    // may_linkat comes before may_create: a refused source in a directory the user cannot write is EPERM.
    assert_eq!(link(&user, b"/a/u/root_ro", b"/a/sub/x"), -EPERM);
    assert_eq!(link(&user, b"/a/u/root_rw", b"/a/sub/x"), -EACCES);
}
