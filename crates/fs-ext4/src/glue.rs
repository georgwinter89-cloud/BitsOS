//! The VFS side of an ext4 volume: `vfs::FsOps` (names on the disk first, the dentry tree follows), one VFS
//! inode per disk inode (hard links share it; the inode number is the disk's, stable across mounts), and
//! mounting a volume into a view.

use crate::dir::FT_DIR;
use crate::file::{Ext4File, Ops};
use crate::inode::{self as di, DiskInode, Stamp};
use crate::vol::{Meta, Volume};
use crate::vol_ns::{rdev_of, Kind};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use crate::errno::*;
use ksync::SpinLock;
use vfs::inode::{Cred, Data, Inode};
use vfs::mount_ns::{Mount, MountNs};
use vfs::setattr::{SetAttr, ATTR_ATIME, ATTR_CTIME, ATTR_GID, ATTR_MODE, ATTR_MTIME, ATTR_UID};
use vfs::{Dentry, SuperBlock};

pub struct Ext4Ops {
    pub vol: Arc<Volume>,
    inodes: SpinLock<Inodes>,
}

#[derive(Default)]
struct Inodes {
    map: BTreeMap<u32, Weak<Inode>>,
    /// Size of the map after the last sweep of dead entries.
    swept: usize,
}

fn ino_of(i: &Inode) -> Result<u32, i64> {
    u32::try_from(i.meta.ino).map_err(|_| EXDEV)
}

impl Ext4Ops {
    pub fn new(vol: Arc<Volume>) -> Arc<Ext4Ops> {
        Arc::new(Ext4Ops { vol, inodes: SpinLock::named("ext4.inodes", Inodes::default()) })
    }

    fn known(&self, ino: u32) -> Option<Arc<Inode>> {
        self.inodes.lock().map.get(&ino).and_then(Weak::upgrade)
    }

    fn remember(&self, ino: u32, i: &Arc<Inode>) {
        let mut t = self.inodes.lock();
        t.map.insert(ino, Arc::downgrade(i));
        if t.map.len() > 2 * t.swept.max(64) {
            t.map.retain(|_, w| w.strong_count() > 0);
            t.swept = t.map.len();
        }
    }

    fn forget_name(&self, ino: u32) {
        self.inodes.lock().map.remove(&ino);
    }

    /// The VFS inode of disk inode `ino` (the one already in use, or a new one from `d`).
    pub fn inode_of(&self, ino: u32, d: Option<DiskInode>, data: Option<&Data>) -> Result<Arc<Inode>, i64> {
        if let Some(i) = self.known(ino) {
            return Ok(i);
        }
        let d = match d {
            Some(d) => d,
            None => self.vol.stat(ino)?,
        };
        let mode = d.mode() as u32;
        let mut file = None;
        let mut fifo = None;
        let data = match (d.ftype(), data) {
            (di::S_IFREG, _) => {
                let f = Ext4File::new(self.vol.clone(), ino, d.size());
                self.vol.hold(ino)?;
                file = Some(f.clone());
                Data::External(Box::new(Ops(f)))
            }
            (di::S_IFDIR, _) => Data::Dir,
            (di::S_IFLNK, _) => Data::Symlink(self.vol.readlink(ino)?),
            (_, Some(Data::Device(f))) => Data::Device(f.clone()),
            (di::S_IFIFO, _) => {
                let f = Arc::new(vfs::pipe::Fifo::new());
                fifo = Some(f.clone());
                Data::Device(f)
            }
            (t @ (di::S_IFCHR | di::S_IFBLK), _) => Data::Device(Arc::new(vfs::special::Special::new(t as u32, rdev_of(&d)))),
            (t, _) => Data::Device(Arc::new(vfs::special::Special::new(t as u32, 0))),
        };
        let i = Inode::new(ino as u64, mode, Cred { uid: d.uid(), gid: d.gid() }, data);
        i.meta.set_times(d.time(Stamp::A), d.time(Stamp::M), d.time(Stamp::C), d.time(Stamp::Cr));
        i.meta.set_nlink(d.links() as u32);
        if let Some(f) = file {
            f.set_inode(&i);
        }
        if let Some(f) = fifo {
            f.attach(&i);
        }
        self.remember(ino, &i);
        Ok(i)
    }
}

impl vfs::FsOps for Ext4Ops {
    fn load_dir(&self, dir: &Arc<Dentry>) -> Result<(), i64> {
        for (name, ino, _) in self.vol.list(ino_of(&dir.inode)?)? {
            let inode = self.inode_of(ino, None, None)?;
            match dir.attach_loaded(&name, inode) {
                Ok(_) | Err(EEXIST) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn create(&self, dir: &Arc<Dentry>, name: &[u8], mode: u32, cred: Cred) -> Result<Arc<Inode>, i64> {
        self.create_node(dir, name, mode, cred, &Data::Dir)
    }

    fn create_node(&self, dir: &Arc<Dentry>, name: &[u8], mode: u32, cred: Cred, data: &Data) -> Result<Arc<Inode>, i64> {
        let kind = match (mode & uapi::S_IFMT, data) {
            (uapi::S_IFREG, _) => Kind::Reg,
            (uapi::S_IFDIR, _) => Kind::Dir,
            (uapi::S_IFLNK, Data::Symlink(t)) => Kind::Symlink(t),
            (uapi::S_IFIFO, _) => Kind::Fifo,
            (t, Data::Device(f)) if t == uapi::S_IFCHR || t == vfs::abi::S_IFBLK => Kind::Dev(f.rdev()),
            (vfs::abi::S_IFSOCK, _) => Kind::Sock,
            _ => return Err(EPERM),
        };
        let d = self.vol.create(ino_of(&dir.inode)?, name, mode as u16, cred.uid, cred.gid, kind)?;
        self.inode_of(d.ino, Some(d), Some(data))
    }

    fn link(&self, dir: &Arc<Dentry>, name: &[u8], inode: &Arc<Inode>) -> Result<(), i64> {
        let ino = ino_of(inode)?;
        // Only an inode of this volume (an O_TMPFILE inode of tmpfs or another mount: EXDEV).
        if !self.known(ino).is_some_and(|k| Arc::ptr_eq(&k, inode)) {
            return Err(EXDEV);
        }
        self.vol.link(ino_of(&dir.inode)?, name, ino)
    }

    fn remove(&self, dir: &Arc<Dentry>, victim: &Arc<Dentry>) -> Result<(), i64> {
        let ino = ino_of(&victim.inode)?;
        self.vol.remove(ino_of(&dir.inode)?, &victim.name(), ino)?;
        if !victim.inode.is_reg() && (victim.is_dir() || victim.inode.meta.nlink() <= 1) {
            self.forget_name(ino);
        }
        Ok(())
    }

    fn rename(&self, odir: &Arc<Dentry>, old: &Arc<Dentry>, ndir: &Arc<Dentry>, nname: &[u8], target: Option<&Arc<Dentry>>) -> Result<(), i64> {
        let t = match target {
            Some(t) => Some(ino_of(&t.inode)?),
            None => None,
        };
        // auto_da_alloc (linux-6.6/fs/ext4/namei.c:3871, VS-2 CR4): a file renamed over another gets its data
        // written first, so a cut after the rename's commit never finds the new name with 0 bytes (the
        // write-then-rename save Brave's profile uses). Linux ignores the result too; the rename goes on.
        if t.is_some()
            && old.inode.is_reg()
            && let Err(e) = old.inode.sync(vfs::pagecache::SyncKind::Background)
        {
            klog::event!(Warn, "ext4", "rename.flush_failed", ino = ino_of(&old.inode)?, errno = e; "the renamed file's data was not written before the rename");
        }
        self.vol.rename(ino_of(&odir.inode)?, &old.name(), ino_of(&old.inode)?, ino_of(&ndir.inode)?, nname, t)?;
        if let (Some(t), Some(td)) = (t, target)
            && !td.inode.is_reg()
            && (td.is_dir() || td.inode.meta.nlink() <= 1)
        {
            self.forget_name(t);
        }
        Ok(())
    }

    fn exchange(&self, a: &Arc<Dentry>, b: &Arc<Dentry>) -> Result<(), i64> {
        let (pa, pb) = (a.parent().ok_or(ENOENT)?, b.parent().ok_or(ENOENT)?);
        self.vol.exchange(ino_of(&pa.inode)?, &a.name(), ino_of(&a.inode)?, ino_of(&pb.inode)?, &b.name(), ino_of(&b.inode)?)
    }

    fn sync(&self) -> Result<(), i64> {
        self.vol.sync_fs()
    }

    /// `freeze_super`'s `sync_filesystem` + `ext4_freeze`: every file's dirty pages written back (their
    /// mappings join the running transaction), then the volume commits, empties its log and marks itself
    /// clean; changes wait until `thaw_fs`.
    fn freeze_fs(&self) -> Result<(), i64> {
        let live: Vec<Arc<Inode>> = self.inodes.lock().map.values().filter_map(Weak::upgrade).collect();
        for i in live.iter().filter(|i| i.is_reg()) {
            i.sync(vfs::pagecache::SyncKind::Background)?;
        }
        self.vol.freeze()
    }

    fn thaw_fs(&self) {
        self.vol.thaw();
    }

    fn can_freeze(&self) -> bool {
        true
    }

    /// `ext4_setattr` without the size part (truncate has its own path): mode, owner and times stored as
    /// given (the VFS checked permissions and fills ctime).
    fn setattr(&self, d: &Dentry, a: &mut SetAttr) -> Result<(), i64> {
        let mut m = Meta::default();
        if a.valid & ATTR_MODE != 0 {
            m.mode = Some((a.mode & 0o7777) as u16);
        }
        if a.valid & ATTR_UID != 0 {
            m.uid = Some(a.uid);
        }
        if a.valid & ATTR_GID != 0 {
            m.gid = Some(a.gid);
        }
        if a.valid & ATTR_ATIME != 0 {
            m.atime = Some(a.atime);
        }
        if a.valid & ATTR_MTIME != 0 {
            m.mtime = Some(a.mtime);
        }
        if a.valid & ATTR_CTIME != 0 {
            m.ctime = Some(a.ctime);
        }
        self.vol.store_meta(ino_of(&d.inode)?, m)
    }

    fn statfs(&self) -> Result<vfs::statfs::StatFs, i64> {
        self.vol.statfs()
    }

    /// st_size and st_blocks of a directory as Linux ext4 shows them (its blocks, `ext4_getattr`).
    fn dir_attr(&self, dir: &Inode) -> Option<(u64, u64)> {
        let d = self.vol.stat(ino_of(dir).ok()?).ok()?;
        Some((d.size(), d.blocks512(&self.vol.geo)))
    }
}

/// A super block and root dentry for `vol` (fstype "ext4").
pub fn super_block(vol: &Arc<Volume>) -> Result<(Arc<SuperBlock>, Arc<Dentry>), i64> {
    let ops = Ext4Ops::new(vol.clone());
    let root = ops.inode_of(di::ROOT, None, None)?;
    if !root.is_dir() {
        return Err(EUCLEAN);
    }
    let sb = SuperBlock::with_ops("ext4", ops);
    // A read-only mount replays a dirty journal in memory (FS-8b): no `norecovery` any more.
    sb.set_options(",errors=remount-ro");
    let d = Dentry::new_root(sb.clone(), root);
    Ok((sb, d))
}

/// Mounts `vol` at `path` of `ns` (the directory is made when missing); read-only when the volume is.
pub fn mount(vol: &Arc<Volume>, ns: &MountNs, path: &str) -> Result<Arc<Mount>, i64> {
    ns.mkdir_p(path)?;
    let at = ns.lookup_path(path.as_bytes(), true)?;
    let (sb, root) = super_block(vol)?;
    ns.mount(&at, sb, root, !vol.writable())
}

/// Every name below a directory of a mounted volume (tests): (path, is_dir).
pub fn names(vol: &Volume, dir: u32) -> Result<Vec<(Vec<u8>, bool)>, i64> {
    Ok(vol.list(dir)?.into_iter().map(|(n, _, ft)| (n, ft == FT_DIR)).collect())
}
