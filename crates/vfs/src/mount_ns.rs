//! Mounts and mount views (B2).
//!
//! A `Mount` puts a file system's root dentry onto a dentry of another mount. A `MountNs` is one view: its
//! root mount and the table "which mount covers (mount, dentry)". Every process holds an `Arc<MountNs>`; G0 and
//! FS-1 give all of them the root view (`vfs::root_ns()`), FS-11 adds `unshare(CLONE_NEWNS)` on
//! `MountNs::copy_view`.

use crate::dentry::{Dentry, SuperBlock};
use crate::Errno;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::errno::*;

static NEXT_MOUNT: AtomicU64 = AtomicU64::new(1);
static NEXT_NS: AtomicU64 = AtomicU64::new(1);

/// AB-7c: per-mount flags (Linux `mnt_flags`, include/linux/mount.h); `readonly` stays its own field.
pub const MNT_NOSUID: u32 = 0x01;
pub const MNT_NODEV: u32 = 0x02;
pub const MNT_NOEXEC: u32 = 0x04;
pub const MNT_NOATIME: u32 = 0x08;
pub const MNT_NODIRATIME: u32 = 0x10;
pub const MNT_RELATIME: u32 = 0x20;
pub const MNT_NOSYMFOLLOW: u32 = 0x80;

pub struct Mount {
    pub id: u64,
    pub sb: Arc<SuperBlock>,
    pub root: Arc<Dentry>,
    /// Where it is mounted (None: the view's root mount).
    pub parent: Option<(Arc<Mount>, Arc<Dentry>)>,
    pub readonly: bool,
    /// `MNT_*` above; MNT_RELATIME by default (Linux `do_new_mount` without an atime option). FS-13 enforces
    /// noexec (execve, mmap PROT_EXEC), nodev (open of device nodes) and nosuid (set-id bits at execve).
    flags: AtomicU32,
}

impl Mount {
    pub fn new(sb: Arc<SuperBlock>, root: Arc<Dentry>, parent: Option<(Arc<Mount>, Arc<Dentry>)>, readonly: bool) -> Arc<Mount> {
        sb.mounts.fetch_add(1, Ordering::AcqRel);
        if let Some((_, d)) = &parent {
            d.mounted.fetch_add(1, Ordering::AcqRel);
        }
        Arc::new(Mount { id: NEXT_MOUNT.fetch_add(1, Ordering::Relaxed), sb, root, parent, readonly, flags: AtomicU32::new(MNT_RELATIME) })
    }
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }
    /// Replaces the `MNT_*` flags (the mounter, right after mounting).
    pub fn set_flags(&self, f: u32) {
        self.flags.store(f, Ordering::Relaxed);
        klog::event!(Debug, "vfs", "mount.flags", mount = self.id, flags = f);
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Some((_, d)) = &self.parent {
            d.mounted.fetch_sub(1, Ordering::AcqRel);
        }
        if self.sb.mounts.fetch_sub(1, Ordering::AcqRel) == 1 {
            Dentry::prune(&self.root);
        }
    }
}

/// A place in a view: (mount, dentry), Linux's `struct path`.
#[derive(Clone)]
pub struct Path {
    pub mnt: Arc<Mount>,
    pub dentry: Arc<Dentry>,
}

impl Path {
    /// Linux `path_noexec` (linux-6.6/fs/exec.c:110): no program runs and nothing maps PROT_EXEC from here.
    pub fn noexec(&self) -> bool {
        self.mnt.flags() & MNT_NOEXEC != 0
    }
    /// Linux `may_open_dev` negated (linux-6.6/fs/namei.c:3226): device nodes here do not open.
    pub fn nodev(&self) -> bool {
        self.mnt.flags() & MNT_NODEV != 0
    }
    /// Linux `path_nosuid`-style test of `bprm_fill_uid` (linux-6.6/fs/exec.c:1611): set-id bits are ignored.
    pub fn nosuid(&self) -> bool {
        self.mnt.flags() & MNT_NOSUID != 0
    }
    pub fn is_mount_root(&self) -> bool {
        Arc::ptr_eq(&self.mnt.root, &self.dentry)
    }
    pub fn same(&self, o: &Path) -> bool {
        Arc::ptr_eq(&self.mnt, &o.mnt) && Arc::ptr_eq(&self.dentry, &o.dentry)
    }
    pub fn attr(&self) -> crate::abi::Attr {
        let mut a = self.dentry.attr();
        a.mnt_id = self.mnt.id;
        a.mount_root = self.is_mount_root();
        a
    }
}

pub struct MountNs {
    pub id: u64,
    root: Arc<Mount>,
    /// (parent mount id, dentry id) -> the mount on top.
    covers: SpinLock<BTreeMap<(u64, u64), Arc<Mount>>>,
}

impl MountNs {
    /// A view whose root is `root` (a mount without parent).
    pub fn with_root(root: Arc<Mount>) -> MountNs {
        MountNs { id: NEXT_NS.fetch_add(1, Ordering::Relaxed), root, covers: SpinLock::named("vfs.mounts", BTreeMap::new()) }
    }
    /// A fresh view with an empty tmpfs as its root (tests; the boot view is `vfs::root_ns()`).
    pub fn new_empty() -> MountNs {
        MountNs::with_root(crate::tmpfs::new_mount(None))
    }
    pub fn root_path(&self) -> Path {
        Path { mnt: self.root.clone(), dentry: self.root.root.clone() }
    }
    /// Every mount of the view: the root mount, then the others by id (mount order); for /proc/<pid>/mounts
    /// (AB-7a CR2).
    pub fn mounts(&self) -> alloc::vec::Vec<Arc<Mount>> {
        let mut v: alloc::vec::Vec<Arc<Mount>> = self.covers.lock().values().cloned().collect();
        v.sort_by_key(|m| m.id);
        v.insert(0, self.root.clone());
        v
    }
    /// The mount covering `p`, if any.
    pub fn covering(&self, p: &Path) -> Option<Arc<Mount>> {
        if p.dentry.mounted.load(Ordering::Acquire) == 0 {
            return None;
        }
        self.covers.lock().get(&(p.mnt.id, p.dentry.id)).cloned()
    }
    /// Steps onto whatever is mounted on `p` (stacked mounts: the topmost).
    pub fn cross_down(&self, mut p: Path) -> Path {
        while let Some(m) = self.covering(&p) {
            p = Path { dentry: m.root.clone(), mnt: m };
        }
        p
    }
    /// Mounts `sb`'s `root` on the directory `at`. Errors: ENOTDIR, EBUSY (the same file system root is
    /// already right there, or `at` is gone).
    pub fn mount(&self, at: &Path, sb: Arc<SuperBlock>, root: Arc<Dentry>, readonly: bool) -> Result<Arc<Mount>, Errno> {
        if !at.dentry.is_dir() || !root.is_dir() {
            return Err(ENOTDIR);
        }
        if at.dentry.is_unlinked() {
            return Err(ENOENT);
        }
        let at = self.cross_down(at.clone());
        let m = Mount::new(sb, root, Some((at.mnt.clone(), at.dentry.clone())), readonly);
        let prev = self.covers.lock().insert((at.mnt.id, at.dentry.id), m.clone());
        debug_assert!(prev.is_none());
        drop(prev);
        klog::event!(Info, "vfs", "mount", ns = self.id, mount = m.id, fs = m.sb.fstype, dev = m.sb.dev, ro = readonly);
        Ok(m)
    }
    /// Unmounts the topmost mount at the mount root `p`. EINVAL when `p` is not a mount root or the view's root,
    /// EBUSY while something is mounted below it.
    pub fn umount(&self, p: &Path) -> Result<(), Errno> {
        if !p.is_mount_root() || Arc::ptr_eq(&p.mnt, &self.root) {
            return Err(EINVAL);
        }
        let (parent, at) = p.mnt.parent.clone().ok_or(EINVAL)?;
        let removed = {
            let mut t = self.covers.lock();
            if t.keys().any(|(pid, _)| *pid == p.mnt.id) {
                return Err(EBUSY);
            }
            t.remove(&(parent.id, at.id))
        };
        match removed {
            Some(m) => {
                klog::event!(Info, "vfs", "umount", ns = self.id, mount = m.id, fs = m.sb.fstype);
                drop(m);
                Ok(())
            }
            None => Err(EINVAL),
        }
    }
    /// Whether some mount sits on `p` in this view (rmdir/rename/unlink answer EBUSY then).
    pub fn is_mountpoint(&self, p: &Path) -> bool {
        self.covering(p).is_some()
    }
    /// A dentry in `mnt`'s file system covered in this view by any mount (for checks on a child name).
    pub fn is_covered(&self, mnt: &Arc<Mount>, d: &Arc<Dentry>) -> bool {
        d.mounted.load(Ordering::Acquire) != 0 && self.covers.lock().contains_key(&(mnt.id, d.id))
    }
    /// `..` of `p` (Linux `follow_dotdot`): at `root` it stays, at a mount root it first steps out to the
    /// mountpoint, then to the parent dentry.
    pub fn parent_of(&self, root: &Path, p: &Path) -> Path {
        let mut p = p.clone();
        loop {
            if p.same(root) {
                return p;
            }
            if !p.is_mount_root() {
                break;
            }
            match p.mnt.parent.clone() {
                Some((m, d)) => p = Path { mnt: m, dentry: d },
                None => return p,
            }
        }
        match p.dentry.parent() {
            Some(d) => self.cross_down(Path { mnt: p.mnt.clone(), dentry: d }),
            None => p,
        }
    }
    /// The absolute path of `p` in this view as Linux's `d_path`/getcwd builds it. `None` when `p` is not
    /// reachable from `root` (another view's mount); the caller decides what to answer then.
    pub fn path_of(&self, root: &Path, p: &Path) -> Option<Vec<u8>> {
        let mut parts: Vec<Vec<u8>> = Vec::new();
        let mut cur = p.clone();
        loop {
            if cur.same(root) {
                break;
            }
            if cur.is_mount_root() {
                match cur.mnt.parent.clone() {
                    Some((m, d)) => {
                        cur = Path { mnt: m, dentry: d };
                        continue;
                    }
                    None => return None,
                }
            }
            match cur.dentry.parent() {
                Some(par) => {
                    parts.push(cur.dentry.name());
                    cur = Path { mnt: cur.mnt.clone(), dentry: par };
                }
                None => return None,
            }
        }
        let mut out = Vec::new();
        for n in parts.iter().rev() {
            out.push(b'/');
            out.extend_from_slice(n);
        }
        if out.is_empty() {
            out.push(b'/');
        }
        Some(out)
    }
    /// FS-10: what Linux's `d_path` shows for `p` seen from `root`: the path in this view, or, for a place on
    /// a mount no view reaches (the internal tmpfs of memfds, Linux's `shm_mnt`), its path from that file
    /// system's root (`/memfd:<name>`). Unlinked names get " (deleted)" as `d_path` adds it.
    pub fn d_path(&self, root: &Path, p: &Path) -> Vec<u8> {
        let mut out = match self.path_of(root, p) {
            Some(v) => v,
            None => {
                let mut parts: Vec<Vec<u8>> = Vec::new();
                let mut d = p.dentry.clone();
                while let Some(par) = d.parent() {
                    parts.push(d.name());
                    d = par;
                }
                let mut v = Vec::new();
                for n in parts.iter().rev() {
                    v.push(b'/');
                    v.extend_from_slice(n);
                }
                if v.is_empty() {
                    v.push(b'/');
                }
                v
            }
        };
        if p.dentry.is_unlinked() {
            out.extend_from_slice(b" (deleted)");
        }
        out
    }
    /// A new view with the same mounts (FS-11's unshare starts here). Mount objects are copied, so later
    /// mounts in one view do not show in the other.
    pub fn copy_view(&self) -> MountNs {
        let new_root = Mount::new(self.root.sb.clone(), self.root.root.clone(), None, self.root.readonly);
        new_root.set_flags(self.root.flags());
        let ns = MountNs::with_root(new_root.clone());
        // Copy parents before children: walk the old table breadth-first from the old root.
        let old: Vec<((u64, u64), Arc<Mount>)> = self.covers.lock().iter().map(|(k, v)| (*k, v.clone())).collect();
        let mut map: BTreeMap<u64, Arc<Mount>> = BTreeMap::new();
        map.insert(self.root.id, new_root);
        let mut queue = alloc::vec![self.root.id];
        while let Some(pid) = queue.pop() {
            for ((p, _), m) in old.iter().filter(|((p, _), _)| *p == pid) {
                let Some(np) = map.get(p).cloned() else { continue };
                let Some((_, at)) = m.parent.clone() else { continue };
                let nm = Mount::new(m.sb.clone(), m.root.clone(), Some((np.clone(), at.clone())), m.readonly);
                nm.set_flags(m.flags());
                ns.covers.lock().insert((np.id, at.id), nm.clone());
                map.insert(m.id, nm);
                queue.push(m.id);
            }
        }
        ns
    }
}
