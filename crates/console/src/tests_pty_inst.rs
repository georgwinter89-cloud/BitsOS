//! Host tests of IN-PTY: ptys of devpts instances (linux-7.3.1/fs/devpts/inode.c `devpts_acquire`,
//! `devpts_pty_new`): numbers per instance, nodes only in their own instance, the wrong instance refused, the
//! node gone with the master, an unmount with a pty open, `max=`, and a ptmx outside devpts finding `pts`.

use crate::pty;
use crate::termios::ioc::*;
use crate::tests_in4::{BASE, Mem};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use uapi::errno::*;
use vfs::OpenFile;
use vfs::mount_fs::{self, Mounter};

const RDWR: u64 = 2;
const NOCTTY: u64 = 0o400;

/// Mounts a new devpts instance (`opts` as mount(2)'s data) on `dir` of the boot view, made by the pty layer
/// as mount(2) makes it (`MountPerms::ptmx` of vfs-sys).
fn mount(dir: &str, opts: &[u8]) -> Arc<vfs::Mount> {
    let ns = vfs::root_ns();
    ns.mkdir_p(dir).unwrap();
    let at = ns.lookup_path(dir.as_bytes(), true).unwrap();
    let o = mount_fs::split(opts).unwrap();
    let same = |n: u32| Some(n);
    let who = Mounter { fsuid: 0, fsgid: 0, uid_in: &same, gid_in: &same, init_ns: true, ns_root: None };
    let d = mount_fs::devpts_opts(&o, &who).unwrap();
    let (sb, root) = mount_fs::devpts(&d, &who, &|i| Some(pty::instance_ptmx(i)));
    ns.mount(&at, sb, root, false).unwrap()
}

fn open(path: &str) -> Result<Arc<OpenFile>, vfs::Errno> {
    vfs::root_ns().open_flags(path.as_bytes(), RDWR | NOCTTY, 0)
}

/// A master opened at `ptmx`, unlocked, and its number.
fn master(ptmx: &str) -> (Arc<OpenFile>, u32) {
    let m = open(ptmx).unwrap_or_else(|e| panic!("open {ptmx}: errno {e}"));
    let mem = Mem::new();
    assert_eq!(m.file.ioctl_user(TIOCGPTN, BASE, &mem), Some(Ok(0)));
    let n = mem.i32_at(0) as u32;
    mem.put(0, &0i32.to_le_bytes());
    assert_eq!(m.file.ioctl_user(TIOCSPTLCK, BASE, &mem), Some(Ok(0)));
    (m, n)
}

fn read(f: &OpenFile) -> String {
    let mut b = vec![0u8; 256];
    let n = f.file.read_stream(&mut b, true).unwrap_or(0);
    String::from_utf8_lossy(&b[..n]).into_owned()
}

fn exists(path: &str) -> bool {
    vfs::root_ns().lookup(path).is_ok()
}

#[test]
fn two_instances_number_from_zero_and_keep_their_nodes_apart() {
    let _a = mount("/tmp/inpty-two/a", b"ptmxmode=666");
    let _b = mount("/tmp/inpty-two/b", b"ptmxmode=666,mode=620,gid=5");
    // `mknod_ptmx`: the instance's ptmx has `ptmxmode` (default 000).
    let mode_of = |p: &str| vfs::root_ns().lookup(p).unwrap().inode.meta.mode();
    assert_eq!(mode_of("/tmp/inpty-two/a/ptmx"), uapi::S_IFCHR | 0o666);
    let _c = mount("/tmp/inpty-two/c", b"");
    assert_eq!(mode_of("/tmp/inpty-two/c/ptmx"), uapi::S_IFCHR, "default ptmxmode=000");
    let (ma, na) = master("/tmp/inpty-two/a/ptmx");
    let (mb, nb) = master("/tmp/inpty-two/b/ptmx");
    assert_eq!((na, nb), (0, 0), "each instance numbers from 0");
    // A new instance's slave is locked until unlockpt (`TTY_PTY_LOCK`): EIO.
    let mc = open("/tmp/inpty-two/c/ptmx").unwrap();
    let mem = Mem::new();
    assert_eq!(mc.file.ioctl_user(TIOCGPTN, BASE, &mem), Some(Ok(0)));
    let nc = mem.i32_at(0);
    assert_eq!(nc, 0);
    assert_eq!(open("/tmp/inpty-two/c/0").err(), Some(EIO), "a locked slave of a new instance opened");
    mem.put(0, &0i32.to_le_bytes());
    assert_eq!(mc.file.ioctl_user(TIOCSPTLCK, BASE, &mem), Some(Ok(0)));
    drop(open("/tmp/inpty-two/c/0").expect("the slave after unlockpt"));
    drop(mc);
    let (ma2, na2) = master("/tmp/inpty-two/a/ptmx");
    assert_eq!(na2, 1);
    assert!(exists("/tmp/inpty-two/a/1") && !exists("/tmp/inpty-two/b/1"), "a's second slave appears in a only");
    // `devpts_pty_new`: owner the opener's fs ids (the kernel here) unless uid=/gid=, mode from mode=.
    let a0 = vfs::root_ns().lookup("/tmp/inpty-two/a/0").unwrap();
    assert_eq!((a0.inode.meta.mode(), a0.inode.meta.gid()), (uapi::S_IFCHR | 0o600, 0));
    let b0 = vfs::root_ns().lookup("/tmp/inpty-two/b/0").unwrap();
    assert_eq!((b0.inode.meta.mode(), b0.inode.meta.gid()), (uapi::S_IFCHR | 0o620, 5));
    // Both live: each slave talks to its own master only.
    let sa = open("/tmp/inpty-two/a/0").unwrap();
    let sb = open("/tmp/inpty-two/b/0").unwrap();
    assert_eq!(ma.file.write_stream(b"to-a\r", false), Ok(5));
    assert_eq!(mb.file.write_stream(b"to-b\r", false), Ok(5));
    assert_eq!(read(&sa), "to-a\n");
    assert_eq!(read(&sb), "to-b\n");
    assert_eq!(sa.file.write_stream(b"x\n", false), Ok(2));
    assert_eq!(read(&mb), "to-b\r\n", "b's master sees only its own echo");
    assert_eq!(read(&ma), "to-a\r\nx\r\n");
    // None of them is the boot instance's, and a's and b's are different instances.
    let ids = |m: &OpenFile| pty::live_pairs().into_iter().find(|p| p.is_master(&*m.file)).map(|p| p.instance_id());
    let (ia, ib, ia2) = (ids(&ma), ids(&mb), ids(&ma2));
    assert!(ia.is_some_and(|i| i != 0) && ib.is_some_and(|i| i != 0) && ia != ib && ia2 == ia, "{ia:?} {ib:?} {ia2:?}");
    drop((sa, sb, ma2));
    drop((ma, mb));
    assert!(!exists("/tmp/inpty-two/a/0") && !exists("/tmp/inpty-two/b/0") && !exists("/tmp/inpty-two/a/1"));
    assert!(!exists("/tmp/inpty-two/c/0"));
    let (ia, ib) = (ia.unwrap(), ib.unwrap());
    assert_eq!((pty::numbers_in(ia), pty::numbers_in(ib)), (0, 0), "numbers stay behind after both ends closed");
}

#[test]
fn the_node_goes_with_the_master_and_the_number_comes_back() {
    let _a = mount("/tmp/inpty-gone", b"ptmxmode=666");
    let (m, n) = master("/tmp/inpty-gone/ptmx");
    let s = open(&format!("/tmp/inpty-gone/{n}")).unwrap();
    drop(m);
    assert!(!exists(&format!("/tmp/inpty-gone/{n}")), "the slave node stays after the master closed");
    assert_eq!(read(&s), "", "a hung-up slave reads 0");
    assert_eq!(s.file.write_stream(b"y", false), Err(EIO));
    drop(s);
    let (m2, n2) = master("/tmp/inpty-gone/ptmx");
    assert_eq!(n2, n, "the lowest free number again");
    let id = pty::live_pairs().into_iter().find(|p| p.is_master(&*m2.file)).map(|p| p.instance_id()).unwrap();
    assert_eq!(pty::numbers_in(id), 1);
    drop(m2);
    assert_eq!(pty::numbers_in(id), 0, "the number stays taken after the master closed");
}

#[test]
fn a_slave_is_opened_only_through_its_own_instance() {
    let _a = mount("/tmp/inpty-wrong/a", b"ptmxmode=666");
    let b = mount("/tmp/inpty-wrong/b", b"ptmxmode=666");
    let (ma, na) = master("/tmp/inpty-wrong/a/ptmx");
    let (ma2, _) = master("/tmp/inpty-wrong/a/ptmx");
    assert!(open(&format!("/tmp/inpty-wrong/b/{}", na + 1)).err() == Some(ENOENT), "b shows a's slave");
    // TIOCGPTPEER opens the slave of the master's own instance.
    let peer = pty::open_peer(&ma, RDWR | NOCTTY).expect("a master").expect("peer");
    assert_eq!(ma.file.write_stream(b"p\r", false), Ok(2));
    assert_eq!(read(&peer), "p\n");
    // A master whose open place is another instance's ptmx: `devpts_mntget` finds no mount of its own instance
    // there (no `pts` next to it: ENOENT, as Linux).
    let elsewhere = vfs::Path { mnt: b.clone(), dentry: b.root.child(b"ptmx").unwrap() };
    let moved = OpenFile::with_path(ma.file.clone(), RDWR, elsewhere);
    assert_eq!(pty::open_peer(&moved, RDWR | NOCTTY).map(|r| r.err()), Some(Some(ENOENT)));
    assert!(pty::open_peer(&peer, RDWR).is_none(), "a slave is no master");
    drop((peer, moved, ma2, ma));
}

#[test]
fn an_unmounted_instance_keeps_its_open_pty_working() {
    let m0 = mount("/tmp/inpty-umount", b"ptmxmode=666");
    // The unmounted tree, held as a process whose cwd is inside it would hold it (Linux keeps it until then).
    let root = m0.root.clone();
    let (m, n) = master("/tmp/inpty-umount/ptmx");
    let name = format!("{n}");
    let s = open(&format!("/tmp/inpty-umount/{n}")).unwrap();
    let ns = vfs::root_ns();
    ns.umount(&vfs::Path { mnt: m0.clone(), dentry: m0.root.clone() }).unwrap();
    drop(m0);
    assert!(!exists(&format!("/tmp/inpty-umount/{n}")), "the unmounted instance is still reachable by path");
    assert!(root.child(name.as_bytes()).is_some(), "the node left the unmounted tree while the master is open");
    assert_eq!(m.file.write_stream(b"u\r", false), Ok(2));
    assert_eq!(read(&s), "u\n");
    let (id, pair) = pty::live_pairs().into_iter().find(|p| p.is_master(&*m.file)).map(|p| (p.instance_id(), Arc::downgrade(&p))).unwrap();
    drop(m);
    assert!(root.child(name.as_bytes()).is_none(), "a stale node in the unmounted tree after the master closed");
    assert_eq!(read(&s), "");
    drop(s);
    assert!(pair.upgrade().is_none(), "the pair is not gone with both ends");
    assert_eq!(pty::numbers_in(id), 0, "the unmounted instance's number stays registered");
    drop(root);
    // A new instance at the same place numbers from 0 again.
    let _m1 = mount("/tmp/inpty-umount", b"ptmxmode=666");
    let (_m2, n2) = master("/tmp/inpty-umount/ptmx");
    assert_eq!(n2, 0);
}

#[test]
fn max_bounds_the_instances_numbers() {
    let _a = mount("/tmp/inpty-max", b"ptmxmode=666,max=1");
    let (_m, n) = master("/tmp/inpty-max/ptmx");
    assert_eq!(n, 0);
    assert_eq!(open("/tmp/inpty-max/ptmx").err(), Some(ENOSPC));
}

/// `devpts_ptmx_path`: a ptmx that is not on devpts (devtmpfs's `/dev/ptmx`, a mknod'd 5:2) makes its pty in the
/// devpts mounted on `pts` next to it; without one the open fails (ENOENT), as on Linux.
#[test]
fn a_ptmx_outside_devpts_takes_the_pts_next_to_it() {
    let ns = vfs::root_ns();
    // The boot ptmx object, registered by number as devfs registers it.
    vfs::special::register(&pty::make_ptmx());
    ns.mkdir_p("/tmp/inpty-sib/dev").unwrap();
    ns.create_special(b"/tmp/inpty-sib/dev/ptmx", uapi::S_IFCHR | 0o666, vfs::abi::encode_dev(5, 2)).unwrap();
    assert_eq!(open("/tmp/inpty-sib/dev/ptmx").err(), Some(ENOENT), "no pts next to it");
    let _p = mount("/tmp/inpty-sib/dev/pts", b"ptmxmode=666");
    let (m, n) = master("/tmp/inpty-sib/dev/ptmx");
    assert_eq!(n, 0);
    assert!(exists("/tmp/inpty-sib/dev/pts/0"));
    assert!(pty::live_pairs().into_iter().any(|p| p.is_master(&*m.file) && p.instance_id() != 0), "made in the boot instance");
    // TIOCGPTPEER of that master: `devpts_mntget` finds the instance on `pts` next to the ptmx.
    let peer = pty::open_peer(&m, RDWR | NOCTTY).expect("a master").expect("peer");
    let node = vfs::root_ns().lookup("/tmp/inpty-sib/dev/pts/0").unwrap();
    assert!(peer.path.as_ref().is_some_and(|p| Arc::ptr_eq(&p.dentry, &node)));
    drop(peer);
    drop(m);
    assert!(!exists("/tmp/inpty-sib/dev/pts/0"));
}

/// A device node made by number (136:N, as mknod(2) by root) in another instance, a plain directory or the boot
/// `/dev` never reaches a new instance's slave: those slaves are registered nowhere by number (Linux:
/// `devpts_get_priv` only answers for a dentry on devpts, else `pts_unix98_lookup` gives EIO).
#[test]
fn a_node_made_by_number_never_reaches_another_instances_slave() {
    let ns = vfs::root_ns();
    let _a = mount("/tmp/inpty-mk/a", b"ptmxmode=666");
    let _b = mount("/tmp/inpty-mk/b", b"ptmxmode=666");
    let (ma, na) = master("/tmp/inpty-mk/a/ptmx");
    let node = |d: Arc<vfs::Dentry>| match &d.inode.data {
        vfs::inode::Data::Device(f) => f.clone(),
        _ => panic!("a's slave is no device node"),
    };
    let ours = node(ns.lookup(&format!("/tmp/inpty-mk/a/{na}")).unwrap());
    let rdev = vfs::abi::encode_dev(pty::PTS_MAJOR, na);
    let (mut made, mut refused) = (vec![], vec![]);
    for path in [format!("/tmp/inpty-mk/b/{na}"), format!("/tmp/inpty-mk/{na}"), format!("/dev/inpty-mk-{na}")] {
        if ns.create_special(path.as_bytes(), uapi::S_IFCHR | 0o600, rdev).is_err() {
            refused.push(path); // refused at mknod: it cannot reach a's slave either
            continue;
        }
        // A boot pty under that number (another test's) is the boot instance's and never a's.
        match vfs::special::lookup(uapi::S_IFCHR, rdev) {
            Some(f) => assert!(!Arc::ptr_eq(&f, &ours), "{path}: a's slave is registered by number"),
            None => assert_eq!(open(&path).err(), Some(ENXIO), "{path}: no device under 136:{na}"),
        }
        ns.remove(&path).unwrap();
        made.push(path);
    }
    // Not vacuous: the plain directory and the boot `/dev` take the node (devpts takes it too today, unlike Linux's
    // EPERM: an open point of the notes).
    assert!(made.len() >= 2, "made {made:?}, refused at mknod {refused:?}");
    drop(ma);
}
