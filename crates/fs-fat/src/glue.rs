//! The VFS side of a FAT volume: `vfs::FsOps` (names on the disk, rules decided by the volume), inodes for
//! FAT nodes, and mounting a volume into a view.

use crate::file::{FatFile, Ops};
use crate::protect::Class;
use crate::vol::{Child, Volume, ROOT};
use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use uapi::errno::*;
use vfs::abi::Time;
use vfs::inode::{Cred, Data, Inode};
use vfs::mount_ns::{Mount, MountNs};
use vfs::setattr::{SetAttr, ATTR_ATIME, ATTR_CTIME, ATTR_GID, ATTR_MODE, ATTR_MTIME, ATTR_UID};
use vfs::{Dentry, SuperBlock};

pub struct FatOps {
    pub vol: Arc<Volume>,
}

fn time(secs: i64) -> Time {
    Time { sec: secs, nsec: 0 }
}

/// The desktop user that owns `\BITSOS2\home\` (YT-2), like a vfat mount with uid=1000,gid=1000.
pub const HOME_OWNER: Cred = Cred { uid: 1000, gid: 1000 };

/// The umask a node shows: 022 (Linux vfat's default), below `\BITSOS2\home\` dmask 077 / fmask 177
/// (linux-6.6/fs/fat/inode.c:518 `fat_make_mode` with those mount options).
fn mask(home: bool, dir: bool) -> u32 {
    match (home, dir) {
        (false, _) => 0o022,
        (true, true) => 0o077,
        (true, false) => 0o177,
    }
}

/// The inode of a FAT node: directories 0755, files 0755 (programs in `\BITSOS2\bin` run), 0555 with the
/// read-only attribute (Linux vfat with the default umask 022 shows the same). Below `\BITSOS2\home\`: owner
/// 1000:1000, directories 0700, files 0600 (0400 read-only).
pub fn inode_of(vol: &Arc<Volume>, c: &Child) -> Arc<Inode> {
    let m = mask(c.home, c.dir);
    let (mode, data) = if c.dir {
        (uapi::S_IFDIR | (0o777 & !m), Data::Dir)
    } else {
        let perm = if c.home { 0o666 } else { 0o777 } & !m & if c.read_only { !0o222 } else { !0 };
        (uapi::S_IFREG | perm, Data::External(Box::new(Ops(FatFile::new(vol.clone(), c.ino, c.size)))))
    };
    let i = Inode::new(c.ino, mode, if c.home { HOME_OWNER } else { Cred::ROOT }, data);
    i.meta.set_times(time(c.atime), time(c.mtime), time(c.mtime), time(c.ctime));
    i
}

impl FatOps {
    /// The whole content of a regular file (CONFIG.TXT checks: small files).
    fn content(d: &Dentry) -> Result<Vec<u8>, i64> {
        if d.inode.size() > crate::protect::CONFIG_MAX {
            return Err(EFBIG);
        }
        let size = d.inode.size() as usize;
        let mut v = Vec::new();
        v.try_reserve_exact(size).map_err(|_| ENOMEM)?;
        v.resize(size, 0);
        let n = d.inode.read_at(0, &mut v)?;
        v.truncate(n);
        Ok(v)
    }
}

impl FatOps {
    /// MC-push CR 4 (INT-W7): after a rename the volume has reclassed the moved node and its subtree
    /// (`Volume::reclass`); the cached inodes take the owner and mode their new class gives, as a fresh lookup
    /// would (`inode_of`), keeping the read-only attribute. Iterative over the cached part only.
    fn reowner(&self, moved: &Arc<Dentry>) {
        let mut work = alloc::vec![moved.clone()];
        while let Some(d) = work.pop() {
            let Ok(class) = self.vol.class(d.inode.meta.ino) else { continue };
            let (home, dir) = (class == Class::Home, d.is_dir());
            let m = &d.inode.meta;
            let owner = if home { HOME_OWNER } else { Cred::ROOT };
            let ro = !dir && m.mode() & 0o200 == 0;
            let base: u32 = if dir || !home { 0o777 } else { 0o666 };
            let perm = base & !mask(home, dir) & if ro { !0o222 } else { !0 };
            if m.uid() != owner.uid || m.gid() != owner.gid || m.mode() & 0o7777 != perm {
                m.set_owner(owner.uid, owner.gid);
                m.set_perm(perm);
                klog::event!(Debug, "fat", "rename.reowner", ino = m.ino, home = home, mode = perm);
            }
            if dir {
                work.extend(d.cached_children());
            }
        }
    }
}

impl vfs::FsOps for FatOps {
    /// Linux's vfat has no byte limit of its own: a name counts 255 UTF-16 units (`xlate_to_uni`,
    /// linux-6.6/fs/fat/namei_vfat.c:525), checked by `name::to_utf16` at create and rename. The VFS lets
    /// through what such a name can be in UTF-8 at most (FAT_LFN_LEN * NLS_MAX_CHARSET_SIZE, the bound vfat
    /// itself sizes its name buffers by, linux-6.6/fs/fat/inode.c:841); a longer lookup is simply not found.
    fn name_max(&self) -> usize {
        255 * 6
    }
    fn fold(&self, name: &[u8]) -> Vec<u8> {
        crate::name::fold(name)
    }

    fn load_dir(&self, dir: &Arc<Dentry>) -> Result<(), i64> {
        for c in self.vol.list(dir.inode.meta.ino)? {
            let inode = inode_of(&self.vol, &c);
            match dir.attach_loaded(&c.name, inode) {
                Ok(_) | Err(EEXIST) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn alias(&self, dir: &Dentry, name: &[u8]) -> Option<Vec<u8>> {
        self.vol.alias(dir.inode.meta.ino, &crate::name::fold(name))
    }

    fn may_write(&self, d: &Dentry) -> Result<(), i64> {
        if d.is_dir() {
            return Ok(());
        }
        self.vol.check_write(d.inode.meta.ino)
    }

    fn may_remove(&self, victim: &Dentry) -> Result<(), i64> {
        let ino = victim.inode.meta.ino;
        let c = self.vol.class(ino)?;
        self.vol.rules().remove(c).map_err(|e| self.vol.refused("remove", ino, e))
    }

    fn create(&self, dir: &Arc<Dentry>, name: &[u8], mode: u32, _cred: Cred) -> Result<Arc<Inode>, i64> {
        let is_dir = match mode & uapi::S_IFMT {
            uapi::S_IFREG => false,
            uapi::S_IFDIR => true,
            // vfat has no symlink, mknod or fifo (no such inode operation: EPERM).
            _ => return Err(EPERM),
        };
        let c = self.vol.create(dir.inode.meta.ino, name, is_dir)?;
        Ok(inode_of(&self.vol, &c))
    }

    fn remove(&self, dir: &Arc<Dentry>, victim: &Arc<Dentry>) -> Result<(), i64> {
        self.vol.remove(dir.inode.meta.ino, victim.inode.meta.ino)
    }

    fn rename(&self, odir: &Arc<Dentry>, old: &Arc<Dentry>, ndir: &Arc<Dentry>, nname: &[u8], target: Option<&Arc<Dentry>>) -> Result<(), i64> {
        let lands_on_config = match target {
            Some(t) => self.vol.class(t.inode.meta.ino)? == Class::Config,
            None => self.vol.class_for_name(ndir.inode.meta.ino, nname)? == Class::Config,
        };
        if lands_on_config && !old.is_dir() {
            let before = match target {
                Some(t) => Self::content(t)?,
                None => Vec::new(),
            };
            let after = Self::content(old)?;
            crate::protect::config_guard(&before, &after).map_err(|key| {
                klog::event!(Warn, "fat", "config.refused", key = key; "rename onto CONFIG.TXT would add or change a guarded line (R8a/R8d)");
                self.vol.refused("config", old.inode.meta.ino, EPERM)
            })?;
        }
        // The moved file's dirty pages go first: its entry is rewritten with the size the disk holds.
        old.inode.sync(vfs::pagecache::SyncKind::Background)?;
        self.vol.rename(odir.inode.meta.ino, old.inode.meta.ino, ndir.inode.meta.ino, nname, target.map(|t| t.inode.meta.ino))?;
        self.reowner(old);
        Ok(())
    }

    fn sync(&self) -> Result<(), i64> {
        self.vol.sync(true)
    }

    /// `fat_setattr` (linux-6.6/fs/fat/file.c:480) without the `quiet` option: owner other than root or a mode
    /// with suid/sgid/sticky bits is EPERM; a mode whose r/x bits differ from what FAT shows is dropped
    /// silently (`fat_sanitize_mode`, umask 022), else the owner's w bit is the read-only attribute; times are
    /// rounded as FAT keeps them (ctime follows mtime, a ctime-only change is ignored; the root's times stay).
    fn setattr(&self, d: &Dentry, a: &mut SetAttr) -> Result<(), i64> {
        let ino = d.inode.meta.ino;
        let home = self.vol.class(ino)? == Class::Home;
        let owner = if home { HOME_OWNER } else { Cred::ROOT };
        if (a.valid & ATTR_UID != 0 && a.uid != owner.uid) || (a.valid & ATTR_GID != 0 && a.gid != owner.gid) || (a.valid & ATTR_MODE != 0 && a.mode & !0o777 != 0) {
            klog::event!(Debug, "fat", "setattr.eperm", ino = ino, valid = a.valid, home = home; "vfat keeps its fixed owner and no suid/sgid/sticky bits");
            return Err(EPERM);
        }
        // The owner it already has: nothing to store (fat_setattr accepts uid == sbi->options.fs_uid).
        a.valid &= !(ATTR_UID | ATTR_GID);
        let mut ro = None;
        if a.valid & ATTR_MODE != 0 {
            let perm = a.mode & 0o777 & !mask(home, d.is_dir());
            let cur = d.inode.meta.mode() & 0o777;
            let w_ok = if d.is_dir() { perm & 0o222 == 0o200 } else { perm & 0o222 == 0 || perm & 0o222 == 0o200 };
            if perm & 0o555 != cur & 0o555 || !w_ok {
                a.valid &= !ATTR_MODE;
            } else {
                a.mode = perm;
                if !d.is_dir() {
                    ro = Some(perm & 0o200 == 0);
                }
            }
        }
        if ino == ROOT {
            a.valid &= !(ATTR_ATIME | ATTR_MTIME | ATTR_CTIME);
            return Ok(());
        }
        let (at, mt) = (a.valid & ATTR_ATIME != 0, a.valid & ATTR_MTIME != 0);
        if at {
            a.atime = time(self.vol.kept_time(a.atime.sec, true));
        }
        if mt {
            a.mtime = time(self.vol.kept_time(a.mtime.sec, false));
            a.ctime = a.mtime;
        } else {
            a.valid &= !ATTR_CTIME;
        }
        // Attribute and times in one entry write: a refusal changes neither the disk nor (by the error) the inode.
        self.vol.set_entry(ino, ro, mt.then_some(a.mtime.sec), at.then_some(a.atime.sec))?;
        Ok(())
    }

    /// `fat_statfs`: clusters as blocks, free clusters, FAT_LFN_LEN * NLS_MAX_CHARSET_SIZE as the name length.
    fn statfs(&self) -> Result<vfs::statfs::StatFs, i64> {
        let (bsize, clusters, free) = self.vol.statfs()?;
        Ok(vfs::statfs::StatFs {
            ftype: vfs::statfs::MSDOS_SUPER_MAGIC,
            bsize: bsize as i64,
            blocks: clusters,
            bfree: free,
            bavail: free,
            namelen: 255 * 6,
            ..Default::default()
        })
    }
}

/// A super block and root dentry for `vol` (fstype "vfat", as Linux names it).
pub fn super_block(vol: &Arc<Volume>) -> (Arc<SuperBlock>, Arc<Dentry>) {
    let sb = SuperBlock::with_ops("vfat", Arc::new(FatOps { vol: vol.clone() }));
    // AB-7c: `fat_show_options` (linux-6.6/fs/fat/inode.c:939) for what V2 does: root-owned, masks 0022 (files
    // and directories 0755), codepage 437 short names, long names as UTF-8, Linux's defaults otherwise.
    sb.set_options(",fmask=0022,dmask=0022,codepage=437,iocharset=ascii,shortname=mixed,utf8,errors=remount-ro");
    let root = Inode::new(ROOT, uapi::S_IFDIR | 0o755, Cred::ROOT, Data::Dir);
    let d = Dentry::new_root(sb.clone(), root);
    (sb, d)
}

/// Mounts `vol` at `path` of `ns` (the directory is made when missing).
pub fn mount(vol: &Arc<Volume>, ns: &MountNs, path: &str, readonly: bool) -> Result<Arc<Mount>, i64> {
    ns.mkdir_p(path)?;
    let at = ns.lookup_path(path.as_bytes(), true)?;
    let (sb, root) = super_block(vol);
    ns.mount(&at, sb, root, readonly)
}
