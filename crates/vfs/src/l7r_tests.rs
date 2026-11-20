//! L7-RESTE-FS: host tests of the FS side of wave L7's change requests.

use crate::abi::{O_CREAT, O_DIRECTORY, O_EXCL, O_PATH, O_RDWR, O_TMPFILE_BIT};
use crate::mount_ns::MountNs;
use crate::ops::Ctx;

/// FMODE_CREATED (linux-7.3.1/fs/namei.c:4561): only the open whose O_CREAT made the file; O_TMPFILE, a later
/// O_CREAT that finds it and O_PATH do not.
#[test]
fn created_only_when_o_creat_made_it() {
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    crate::tmpfs::mount_new(&ns, "/t", 0o1777, None).unwrap();
    let made = c.open(None, b"/t/a", O_CREAT | O_EXCL | O_RDWR, 0o600).unwrap();
    assert!(made.created());
    assert!(!c.open(None, b"/t/a", O_CREAT | O_RDWR, 0o600).unwrap().created());
    assert!(!c.open(None, b"/t/a", O_RDWR, 0).unwrap().created());
    assert!(!c.open(None, b"/t/a", O_PATH, 0).unwrap().created());
    assert!(!c.open(None, b"/t", O_TMPFILE_BIT | O_DIRECTORY | O_RDWR, 0o600).unwrap().created());
}

/// A directory with its own permission op that refuses (failfs's EOPNOTSUPP).
struct Refuses;

impl crate::synth::DirSource for Refuses {
    fn entries(&self) -> Result<alloc::vec::Vec<crate::synth::SynthEntry>, crate::Errno> {
        Ok(alloc::vec::Vec::new())
    }
    fn permission(&self, _who: crate::Cred, _want: u32) -> Option<Result<(), crate::Errno>> {
        Some(Err(uapi::errno::EOPNOTSUPP))
    }
    fn jumped_to(&self) -> Result<(), crate::Errno> {
        Err(uapi::errno::EOPNOTSUPP)
    }
}

/// `DirSource::permission` is the directory's `inode_permission` (walks through it, chdir) and `jumped_to` its
/// `d_weak_revalidate` (a walk of "/" that ends on it); a source without them keeps the mode bits.
#[test]
fn synthetic_directory_permission_op() {
    use crate::inode::{Data, Inode, MAY_EXEC};
    use crate::synth::SynthDir;
    use alloc::sync::Arc;
    use uapi::errno::EOPNOTSUPP;
    let ns = MountNs::new_empty();
    let base = ns.root_path();
    let sd = SynthDir::new();
    sd.add(Arc::new(Refuses));
    let inode = Inode::new(7, uapi::S_IFDIR | 0o555, crate::Cred::ROOT, Data::SynthDir(Arc::new(sd)));
    let root = crate::Path { mnt: base.mnt.clone(), dentry: crate::dentry::Dentry::ephemeral(&base.dentry, b"r", inode) };
    assert_eq!(root.dentry.inode.permission(crate::Cred::ROOT, MAY_EXEC), Err(EOPNOTSUPP), "even root");
    let user = crate::Cred { uid: 1000, gid: 1000 };
    let mut w = crate::walk::Walker::new(&ns, root.clone(), user);
    assert_eq!(w.lookup(root.clone(), b"/x", true).err(), Some(EOPNOTSUPP), "a walk through it");
    assert_eq!(w.lookup(root.clone(), b"/..", true).err(), Some(EOPNOTSUPP), "dot-dot is a component");
    assert_eq!(w.lookup(root.clone(), b"/", true).err(), Some(EOPNOTSUPP), "a jump onto it");
    assert_eq!(crate::ops::Ctx::dir_usable(&root, user), Err(EOPNOTSUPP), "chdir");
    let plain = crate::Path { mnt: base.mnt.clone(), dentry: crate::dentry::Dentry::ephemeral(&base.dentry, b"p", Inode::new(8, uapi::S_IFDIR | 0o700, crate::Cred::ROOT, Data::SynthDir(Arc::new(SynthDir::new())))) };
    assert_eq!(plain.dentry.inode.permission(user, MAY_EXEC), Err(uapi::errno::EACCES), "no op: the mode bits");
    assert!(crate::walk::Walker::new(&ns, plain.clone(), user).lookup(plain.clone(), b"/", true).is_ok());
}

/// CLONE_EMPTY_MNTNS (`copy_mnt_ns`, linux-7.3.1/fs/namespace.c:4420): one copy of the root mount, nothing
/// below it; a peer of a shared root for the same owner, a locked slave for another.
#[test]
fn copy_root_only_holds_the_root_alone() {
    use crate::mount_ns::MNT_LOCKED;
    use crate::mount_ops::Propagation;
    let ns = MountNs::new_empty();
    let c = Ctx::kernel(&ns);
    c.mkdir(None, b"/a", 0o755).unwrap();
    c.open(None, b"/f", O_CREAT | O_RDWR, 0o600).unwrap();
    crate::tmpfs::mount_new(&ns, "/a", 0o755, None).unwrap();
    let top = ns.root_path();
    ns.change_type(&top, Propagation::Shared, false).unwrap();
    let same = ns.copy_root_only(ns.owner_key());
    assert_eq!(same.mounts().len(), 1);
    let r = same.root_mount();
    assert!(alloc::sync::Arc::ptr_eq(&r.sb, &top.mnt.sb), "a copy of the root mount");
    assert_eq!(r.propagation().0, top.mnt.propagation().0, "a peer of the shared root");
    let cs = Ctx::kernel(&same);
    assert!(cs.lookup(None, b"/f", true).is_ok(), "the root's own names stay");
    assert!(cs.lookup(None, b"/a", true).unwrap().mnt.id == r.id, "/a is the plain directory: nothing mounted");
    let other = ns.copy_root_only(9);
    let o = other.root_mount();
    assert_eq!(o.propagation(), (0, top.mnt.propagation().0), "a slave of the root's group");
    assert!(o.flags() & MNT_LOCKED != 0, "locked for another owner");
    assert_eq!(other.mounts().len(), 1);
}
