//! SX-3a: a read-only overlay (linux-6.6/fs/overlayfs: `ovl_lookup`, `ovl_iterate`'s merge, lower layers only):
//! `mount -t overlay -o lowerdir=top:...:bottom`. A directory shows the union of the layers' directories of
//! that name, the upper layer first; a non-directory hides everything below it, a whiteout (a character device
//! 0:0) hides the name; files are the lower layers' own inodes, so reading them reads the layer. The file
//! system is read-only (Linux sets SB_RDONLY without an upper layer). Lower layers are walked by their
//! dentries, not across mounts below them (as `ovl_lookup_single` does).
//!
//! Not built: an upper layer (writes, copy-up, whiteouts made here): upperdir= is EINVAL with an event; the
//! opaque-directory xattr (V2 keeps no xattrs on tmpfs).

use crate::dentry::{Dentry, SuperBlock};
use crate::inode::{Cred, Data, Inode};
use crate::mount_ns::Path;
use crate::Errno;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use uapi::errno::*;

/// `OVERLAYFS_SUPER_MAGIC`.
const OVERLAY_MAGIC: i64 = 0x794c_7630;

struct Overlay {
    /// Overlay directory dentry id -> the layers' directories it merges, upper first.
    dirs: SpinLock<BTreeMap<u64, Vec<Arc<Dentry>>>>,
    top: Arc<SuperBlock>,
}

fn whiteout(d: &Dentry) -> bool {
    let a = d.inode.attr(0, 0);
    a.mode & uapi::S_IFMT == uapi::S_IFCHR && a.rdev == 0
}

impl Overlay {
    fn dir_inode(sb: &SuperBlock, like: &Dentry) -> Arc<Inode> {
        let m = &like.inode.meta;
        Inode::new(sb.next_ino(), uapi::S_IFDIR | (m.mode() & 0o7777), Cred { uid: m.uid(), gid: m.gid() }, Data::Dir)
    }
}

impl crate::fsops::FsOps for Overlay {
    fn load_dir(&self, dir: &Arc<Dentry>) -> Result<(), Errno> {
        let layers = self.dirs.lock().get(&dir.id).cloned().unwrap_or_default();
        // Names a layer decided for the layers below: a file, a whiteout, the end of a merge.
        let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
        // name -> the layers' directories of that name (a merged directory); files of the first layer that has one.
        let mut merged: BTreeMap<Vec<u8>, Vec<Arc<Dentry>>> = BTreeMap::new();
        let mut files: Vec<(Vec<u8>, Arc<Dentry>)> = Vec::new();
        for l in layers.iter() {
            l.load()?;
            for (_, c) in l.children_from(0, usize::MAX) {
                let name = c.name();
                if seen.contains(&name) {
                    continue;
                }
                if let Some(v) = merged.get_mut(&name) {
                    if c.is_dir() {
                        v.push(c);
                    } else {
                        seen.insert(name);
                    }
                } else if whiteout(&c) {
                    seen.insert(name);
                } else if c.is_dir() {
                    merged.insert(name, alloc::vec![c]);
                } else {
                    seen.insert(name.clone());
                    files.push((name, c));
                }
            }
        }
        let sb = dir.sb.clone();
        for (name, c) in files {
            dir.attach_loaded(&name, c.inode.clone())?;
        }
        for (name, v) in merged {
            let d = dir.attach_loaded(&name, Self::dir_inode(&sb, &v[0]))?;
            self.dirs.lock().insert(d.id, v);
        }
        Ok(())
    }
    fn may_write(&self, _d: &Dentry) -> Result<(), Errno> {
        Err(EROFS)
    }
    fn may_remove(&self, _victim: &Dentry) -> Result<(), Errno> {
        Err(EROFS)
    }
    fn create(&self, _dir: &Arc<Dentry>, _name: &[u8], _mode: u32, _cred: Cred) -> Result<Arc<Inode>, Errno> {
        Err(EROFS)
    }
    fn remove(&self, _dir: &Arc<Dentry>, _victim: &Arc<Dentry>) -> Result<(), Errno> {
        Err(EROFS)
    }
    fn rename(&self, _odir: &Arc<Dentry>, _old: &Arc<Dentry>, _ndir: &Arc<Dentry>, _nname: &[u8], _target: Option<&Arc<Dentry>>) -> Result<(), Errno> {
        Err(EROFS)
    }
    fn statfs(&self) -> Result<crate::statfs::StatFs, Errno> {
        let mut s = crate::statfs::of(&self.top, true)?;
        s.ftype = OVERLAY_MAGIC;
        Ok(s)
    }
}

/// overlay's options: the lower layers (top first, ':' between, "\:" for a colon in a name) and the
/// flags taken as Linux takes them. EINVAL: no lowerdir, fewer than two layers without an upper one, an upper
/// layer (not built), an unknown option.
pub fn parse(o: &crate::mount_fs::Opts) -> Result<(Vec<Vec<u8>>, String), Errno> {
    let mut lower: Option<Vec<u8>> = None;
    let mut show = String::new();
    for (k, v) in o.rest.iter() {
        match (k.as_slice(), v.as_deref()) {
            (b"lowerdir", Some(l)) if !l.is_empty() => lower = Some(l.to_vec()),
            (b"upperdir" | b"workdir", Some(_)) => {
                klog::event!(Info, "vfs", "overlay.upper_unsupported"; "V2's overlay has no upper layer yet");
                return Err(EINVAL);
            }
            (b"userxattr" | b"default_permissions" | b"volatile", None) => {
                show.push(',');
                show.push_str(core::str::from_utf8(k).unwrap_or(""));
            }
            (b"redirect_dir", Some(b"on" | b"off" | b"follow" | b"nofollow")) | (b"index" | b"nfs_export" | b"metacopy", Some(b"on" | b"off")) | (b"xino", Some(b"on" | b"off" | b"auto")) | (b"uuid", Some(b"on" | b"off" | b"null" | b"auto")) => {}
            _ => return Err(EINVAL),
        }
    }
    let l = lower.ok_or(EINVAL)?;
    let mut layers: Vec<Vec<u8>> = alloc::vec![Vec::new()];
    let mut esc = false;
    for &b in l.iter() {
        match (esc, b) {
            (false, b'\\') => esc = true,
            (false, b':') => layers.push(Vec::new()),
            (_, c) => {
                esc = false;
                if let Some(x) = layers.last_mut() {
                    x.push(c);
                }
            }
        }
    }
    if layers.iter().any(|x| x.is_empty()) || layers.len() < 2 {
        return Err(EINVAL);
    }
    let mut s = String::from(",lowerdir=");
    s.push_str(&String::from_utf8_lossy(&l));
    s.push_str(&show);
    Ok((layers, s))
}

/// A new read-only overlay of `layers` (looked up by the caller, directories, upper first).
pub fn new(layers: Vec<Path>, options: &str) -> (Arc<SuperBlock>, Arc<Dentry>) {
    let top = layers[0].dentry.sb.clone();
    let ops = Arc::new(Overlay { dirs: SpinLock::named("vfs.overlay.dirs", BTreeMap::new()), top });
    let sb = SuperBlock::with_ops("overlay", ops.clone());
    sb.set_options(options);
    sb.set_ro(true);
    let root = Dentry::new_root(sb.clone(), Overlay::dir_inode(&sb, &layers[0].dentry));
    ops.dirs.lock().insert(root.id, layers.into_iter().map(|p| p.dentry).collect());
    (sb, root)
}
