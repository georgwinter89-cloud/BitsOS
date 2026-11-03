//! Mounts and mount views (B2; SX-3a builds FS-11's views with Linux's rules).
//!
//! A `Mount` puts a file system's dentry (its root, or any directory or file of it for a bind mount) onto a
//! dentry of another mount. A `MountNs` is one view (Linux `mnt_namespace`): its root mount and every mount
//! attached below it. As in Linux (linux-6.6/fs/namespace.c `mount_hashtable`) the table "which mount sits on
//! (mount, dentry)" is one for the whole system: mount ids never repeat, so a key names one view's place, and a
//! walk crosses mounts without knowing the view (a cwd or an open directory of another view keeps walking in
//! that view, as on Linux). A view's root is kept in the same table under the key (0, view id).
//!
//! Mutable mount state (where it sits, its view, its peer group and master) lives in each `Mount` behind a leaf
//! lock and changes only under `TOPO` (Linux `namespace_sem`). Nothing is dropped while `TOPO` is held: removed
//! mounts and dentries go into a `Grave` that is dropped after the lock. Path walks never take `TOPO`: they look
//! the places up in `COVER`, a leaf copy of the table's places (Linux walks the mount hash under `mount_lock`'s
//! seqcount), so a long change or a long /proc read stalls other mount calls only, never a walk.
//!
//! No work under `TOPO` scans the whole system's mounts (R1 allows any number, and a user namespace's root can
//! make them): a view's mounts are a walk from its root, a peer group's members and slaves are ranges of the
//! table's indexes (`groups`, `masters`).
//!
//! Propagation (linux-6.6/fs/pnode.c) is kept by peer group ids: `group` is the mount's peer group ("shared:N"),
//! `master` the peer group it receives from ("master:N"). The tree operations are in `mount_tree.rs`, the
//! calls of mount(2), umount2(2) and pivot_root(2) in `mount_ops.rs`, /proc's two files in `mountinfo.rs`.

use crate::dentry::{Dentry, SuperBlock};
use crate::Errno;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ksync::SpinLock;
use uapi::errno::*;

static NEXT_MOUNT: AtomicU64 = AtomicU64::new(1);
static NEXT_NS: AtomicU64 = AtomicU64::new(1);
static NEXT_GROUP: AtomicU64 = AtomicU64::new(1);
/// Views made by `copy_for` that still live (callers skip per-call work while there are none).
static COPIES: AtomicUsize = AtomicUsize::new(0);

/// AB-7c: per-mount flags (Linux `mnt_flags`, include/linux/mount.h).
pub const MNT_NOSUID: u32 = 0x01;
pub const MNT_NODEV: u32 = 0x02;
pub const MNT_NOEXEC: u32 = 0x04;
pub const MNT_NOATIME: u32 = 0x08;
pub const MNT_NODIRATIME: u32 = 0x10;
pub const MNT_RELATIME: u32 = 0x20;
pub const MNT_READONLY: u32 = 0x40;
pub const MNT_NOSYMFOLLOW: u32 = 0x80;
pub const MNT_UNBINDABLE: u32 = 0x2000;
pub const MNT_LOCK_ATIME: u32 = 0x04_0000;
pub const MNT_LOCK_NOEXEC: u32 = 0x08_0000;
pub const MNT_LOCK_NOSUID: u32 = 0x10_0000;
pub const MNT_LOCK_NODEV: u32 = 0x20_0000;
pub const MNT_LOCK_READONLY: u32 = 0x40_0000;
/// A mount an unprivileged view got from its parent view: it cannot be unmounted alone (it would uncover what
/// the parent view's owner hid).
pub const MNT_LOCKED: u32 = 0x80_0000;
pub const MNT_ATIME_MASK: u32 = MNT_NOATIME | MNT_NODIRATIME | MNT_RELATIME;
/// `MNT_USER_SETTABLE_MASK`.
pub const MNT_USER_SETTABLE: u32 = MNT_NOSUID | MNT_NODEV | MNT_NOEXEC | MNT_ATIME_MASK | MNT_READONLY | MNT_NOSYMFOLLOW;

/// What changes about a mount after it was made (under `TOPO`).
#[derive(Clone, Default)]
pub(crate) struct St {
    /// Where it sits (None: a view's root or a mount in no view).
    pub(crate) mp: Option<(Arc<Mount>, Arc<Dentry>)>,
    /// Its view (0: none: made, but not attached yet, or unmounted).
    pub(crate) ns: u64,
    /// Peer group (0: not shared).
    pub(crate) group: u64,
    /// The peer group it is a slave of (0: none).
    pub(crate) master: u64,
    /// umount2(MNT_EXPIRE)'s mark.
    pub(crate) expire: bool,
}

pub struct Mount {
    pub id: u64,
    pub sb: Arc<SuperBlock>,
    pub root: Arc<Dentry>,
    /// `MNT_*` above; MNT_RELATIME by default (Linux `do_new_mount` without an atime option). FS-13 enforces
    /// noexec (execve, mmap PROT_EXEC), nodev (open of device nodes) and nosuid (set-id bits at execve).
    flags: AtomicU32,
    pub(crate) st: SpinLock<St>,
    /// mount(2)'s source (Linux `mnt_devname`, per mount: binds and copies keep it); None: the super block's.
    devname: SpinLock<Option<alloc::string::String>>,
}

impl Mount {
    /// A mount of `sb` at `root`, not attached anywhere (`mountpoint()` says where it sits once grafted);
    /// `readonly` sets MNT_READONLY (`is_readonly()` is the answer later).
    pub fn new(sb: Arc<SuperBlock>, root: Arc<Dentry>, readonly: bool) -> Arc<Mount> {
        sb.mounts.fetch_add(1, Ordering::AcqRel);
        let f = MNT_RELATIME | if readonly { MNT_READONLY } else { 0 };
        Arc::new(Mount { id: NEXT_MOUNT.fetch_add(1, Ordering::Relaxed), sb, root, flags: AtomicU32::new(f), st: SpinLock::named("vfs.mount", St::default()), devname: SpinLock::named("vfs.mount.devname", None) })
    }
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }
    pub(crate) fn store_flags(&self, f: u32) {
        self.flags.store(f, Ordering::Release);
    }
    /// Replaces the settable `MNT_*` flags (the kernel mounter, right after mounting); read-only, the locks and
    /// unbindable stay.
    pub fn set_flags(&self, f: u32) {
        let keep = MNT_READONLY | !MNT_USER_SETTABLE;
        let new = (self.flags() & keep) | (f & MNT_USER_SETTABLE & !MNT_READONLY);
        self.store_flags(new);
        klog::event!(Debug, "vfs", "mount.flags", mount = self.id, flags = new);
    }
    /// Adds lock bits (`MNT_LOCK_*`) a new mount takes over from a visible one (`mnt_already_visible`).
    pub(crate) fn set_locks(&self, locks: u32) {
        self.store_flags(self.flags() | locks);
    }
    /// The source /proc's mount files show (`mnt_devname`, else the super block's source).
    pub fn source(&self) -> alloc::string::String {
        self.devname.lock().clone().unwrap_or_else(|| self.sb.source())
    }
    pub(crate) fn set_source(&self, s: Option<alloc::string::String>) {
        *self.devname.lock() = s;
    }
    pub(crate) fn devname(&self) -> Option<alloc::string::String> {
        self.devname.lock().clone()
    }
    /// Linux `__mnt_is_readonly`: the mount or its file system is read-only.
    pub fn is_readonly(&self) -> bool {
        self.flags() & MNT_READONLY != 0 || self.sb.is_ro()
    }
    /// Where it sits now: (parent mount, mountpoint dentry).
    pub fn mountpoint(&self) -> Option<(Arc<Mount>, Arc<Dentry>)> {
        self.st.lock().mp.clone()
    }
    /// The view it belongs to (0: none).
    pub fn ns_id(&self) -> u64 {
        self.st.lock().ns
    }
    /// (peer group, master group), 0 for none.
    pub fn propagation(&self) -> (u64, u64) {
        let s = self.st.lock();
        (s.group, s.master)
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if self.sb.mounts.fetch_sub(1, Ordering::AcqRel) == 1 {
            // The file system's last mount: break the whole dentry tree's cycles (a bind mount's root may be a
            // directory inside it).
            let mut top = self.root.clone();
            while let Some(p) = top.parent() {
                top = p;
            }
            Dentry::prune(&top);
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
    /// No caller yet: V2 has no set-id exec; when it comes, exec calls it next to `noexec` (CONTRACTS §7a).
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

/// The system's mount table (see the module doc).
pub(crate) struct Topo {
    /// (parent mount id, mountpoint dentry id) -> the mount there; (0, view id) -> the view's root. Changed only
    /// through `put` and `take`, which keep the indexes and `COVER` in step.
    pub(crate) hash: BTreeMap<(u64, u64), Arc<Mount>>,
    /// View id -> its owner key (the owning user namespace's inode number; 0 = the initial one).
    pub(crate) owners: BTreeMap<u64, u32>,
    /// Mount id -> its key in `hash`, for every mount in the table.
    keys: BTreeMap<u64, (u64, u64)>,
    /// (peer group, mount id) and (master group, mount id) of the mounts in the table (weak: the table's own
    /// reference is the one in `hash`, so a busy check's reference count stays as it was).
    groups: BTreeMap<(u64, u64), Weak<Mount>>,
    masters: BTreeMap<(u64, u64), Weak<Mount>>,
}

pub(crate) static TOPO: SpinLock<Topo> = SpinLock::named("vfs.mounts", Topo { hash: BTreeMap::new(), owners: BTreeMap::new(), keys: BTreeMap::new(), groups: BTreeMap::new(), masters: BTreeMap::new() });

/// The places of `hash` (parent mount id != 0) for path walks: taken alone, or innermost under `TOPO`.
static COVER: SpinLock<BTreeMap<(u64, u64), Weak<Mount>>> = SpinLock::named("vfs.mounts.cover", BTreeMap::new());

fn range(m: &BTreeMap<(u64, u64), Weak<Mount>>, k: u64) -> impl Iterator<Item = (u64, &Weak<Mount>)> {
    m.range((k, 0)..=(k, u64::MAX)).map(|(&(_, id), w)| (id, w))
}

impl Topo {
    /// Puts `m` at `key` (`m` is in no other place of the table); returns what sat there.
    pub(crate) fn put(&mut self, key: (u64, u64), m: &Arc<Mount>) -> Option<Arc<Mount>> {
        let old = self.hash.insert(key, m.clone());
        if let Some(o) = &old {
            self.unindex(o);
        }
        self.keys.insert(m.id, key);
        let (g, ms) = m.propagation();
        if g != 0 {
            self.groups.insert((g, m.id), Arc::downgrade(m));
        }
        if ms != 0 {
            self.masters.insert((ms, m.id), Arc::downgrade(m));
        }
        if key.0 != 0 {
            COVER.lock().insert(key, Arc::downgrade(m));
        }
        old
    }
    /// Takes whatever sits at `key` out of the table.
    pub(crate) fn take(&mut self, key: (u64, u64)) -> Option<Arc<Mount>> {
        let old = self.hash.remove(&key)?;
        self.unindex(&old);
        if key.0 != 0 {
            COVER.lock().remove(&key);
        }
        Some(old)
    }
    fn unindex(&mut self, m: &Arc<Mount>) {
        self.keys.remove(&m.id);
        let (g, ms) = m.propagation();
        self.groups.remove(&(g, m.id));
        self.masters.remove(&(ms, m.id));
    }
    /// Whether `m` is in the table.
    pub(crate) fn has(&self, m: &Mount) -> bool {
        self.keys.contains_key(&m.id)
    }
    /// Sets `m`'s peer group and master, the indexes with them.
    pub(crate) fn set_prop(&mut self, m: &Arc<Mount>, group: u64, master: u64) {
        let (g0, m0) = m.propagation();
        if (g0, m0) == (group, master) {
            return;
        }
        {
            let mut st = m.st.lock();
            st.group = group;
            st.master = master;
        }
        if self.has(m) {
            self.groups.remove(&(g0, m.id));
            self.masters.remove(&(m0, m.id));
            if group != 0 {
                self.groups.insert((group, m.id), Arc::downgrade(m));
            }
            if master != 0 {
                self.masters.insert((master, m.id), Arc::downgrade(m));
            }
        }
    }
    /// The table's members of peer group `group` (every one is alive: the table holds it).
    pub(crate) fn peers(&self, group: u64) -> Vec<Arc<Mount>> {
        range(&self.groups, group).filter_map(|(_, w)| w.upgrade()).collect()
    }
    /// The table's slaves of peer group `group`.
    pub(crate) fn slaves(&self, group: u64) -> Vec<Arc<Mount>> {
        range(&self.masters, group).filter_map(|(_, w)| w.upgrade()).collect()
    }
    /// Whether peer group `group` has a member in the table other than mount `id`.
    pub(crate) fn has_other_peer(&self, group: u64, id: u64) -> bool {
        range(&self.groups, group).any(|(i, _)| i != id)
    }
    /// View `ns`'s mounts, parents before children (a walk from its root: the cost is the view's, not the
    /// system's).
    pub(crate) fn view(&self, ns: u64) -> Vec<Arc<Mount>> {
        match self.hash.get(&(0, ns)) {
            Some(r) => crate::mount_tree::subtree(self, r).into_iter().filter(|m| m.ns_id() == ns).collect(),
            None => Vec::new(),
        }
    }
}

/// What a change under `TOPO` let go of; dropped after the lock (a mount's or dentry's drop may take other locks).
#[derive(Default)]
pub(crate) struct Grave {
    pub(crate) mounts: Vec<Arc<Mount>>,
    pub(crate) places: Vec<(Arc<Mount>, Arc<Dentry>)>,
}

pub(crate) fn new_group() -> u64 {
    NEXT_GROUP.fetch_add(1, Ordering::Relaxed)
}

/// Where a view was copied from: the source view and "source mount id -> its copy" (`copy_mnt_ns` re-points
/// the copier's root and cwd through it; `FsState::enter` does it for those who come later).
struct Source {
    ns: u64,
    map: BTreeMap<u64, Weak<Mount>>,
}

pub struct MountNs {
    pub id: u64,
    /// The id mountinfo shows as the parent of the view's root: a mount id no mount gets. Linux always has the
    /// namespace's rootfs below `/` (hidden, as no reader's root reaches it); V2's views have none.
    base: u64,
    root: SpinLock<Arc<Mount>>,
    owner: u32,
    source: Option<Source>,
}

impl MountNs {
    /// A view whose root is `root` (a mount without parent), owned by the initial user namespace.
    pub fn with_root(root: Arc<Mount>) -> MountNs {
        Self::build(NEXT_NS.fetch_add(1, Ordering::Relaxed), root, 0, None)
    }
    fn build(id: u64, root: Arc<Mount>, owner: u32, source: Option<Source>) -> MountNs {
        {
            let mut t = TOPO.lock();
            root.st.lock().ns = id;
            t.put((0, id), &root);
            t.owners.insert(id, owner);
        }
        if source.is_some() {
            COPIES.fetch_add(1, Ordering::Relaxed);
        }
        MountNs { id, base: NEXT_MOUNT.fetch_add(1, Ordering::Relaxed), root: SpinLock::named("vfs.ns.root", root), owner, source }
    }
    /// A fresh view with an empty tmpfs as its root (tests; the boot view is `vfs::root_ns()`).
    pub fn new_empty() -> MountNs {
        MountNs::with_root(crate::tmpfs::new_mount())
    }
    /// Whether any copied view lives (else every process uses the boot view or a test's own).
    pub fn copies_exist() -> bool {
        COPIES.load(Ordering::Relaxed) != 0
    }
    pub fn root_mount(&self) -> Arc<Mount> {
        self.root.lock().clone()
    }
    pub fn root_path(&self) -> Path {
        let m = self.root_mount();
        Path { dentry: m.root.clone(), mnt: m }
    }
    pub(crate) fn set_root_mount(&self, m: Arc<Mount>) -> Arc<Mount> {
        core::mem::replace(&mut *self.root.lock(), m)
    }
    /// See `base`.
    pub(crate) fn base_id(&self) -> u64 {
        self.base
    }
    /// The owning user namespace's key (0: the initial one).
    pub fn owner_key(&self) -> u32 {
        self.owner
    }
    /// Linux `check_mnt`: `m` is attached in this view.
    pub fn check_mnt(&self, m: &Mount) -> bool {
        m.ns_id() == self.id
    }
    /// Every mount of the view in mount order (ids grow with every mount and copy; Linux's `ns->list` order).
    pub fn mounts(&self) -> Vec<Arc<Mount>> {
        let mut v = TOPO.lock().view(self.id);
        v.sort_by_key(|m| m.id);
        v
    }
    /// The mount covering `p`, if any.
    pub fn covering(&self, p: &Path) -> Option<Arc<Mount>> {
        covering(p)
    }
    /// Steps onto whatever is mounted on `p` (stacked mounts: the topmost).
    pub fn cross_down(&self, p: Path) -> Path {
        cross_down(p)
    }
    /// Whether some mount sits on `p` (rmdir/rename/unlink answer EBUSY then).
    pub fn is_mountpoint(&self, p: &Path) -> bool {
        covering(p).is_some()
    }
    /// A dentry in `mnt`'s file system covered by any mount (for checks on a child name).
    pub fn is_covered(&self, mnt: &Arc<Mount>, d: &Arc<Dentry>) -> bool {
        d.mounted.load(Ordering::Acquire) != 0 && COVER.lock().contains_key(&(mnt.id, d.id))
    }
    /// The kernel's own mount (boot, data volumes, snapshots): `sb`'s `root` on the directory `at` (the topmost
    /// mount there), read-only as asked, with propagation like mount(2). Errors: ENOTDIR, ENOENT (`at` is gone),
    /// EINVAL (`at` is not in this view).
    pub fn mount(&self, at: &Path, sb: Arc<SuperBlock>, root: Arc<Dentry>, readonly: bool) -> Result<Arc<Mount>, Errno> {
        if !at.dentry.is_dir() || !root.is_dir() {
            return Err(ENOTDIR);
        }
        let at = cross_down(at.clone());
        if at.dentry.is_unlinked() {
            return Err(ENOENT);
        }
        if sb.mounts.load(Ordering::Acquire) == 0 && readonly {
            sb.set_ro(true);
        }
        let m = Mount::new(sb, root, readonly);
        let mut g = Grave::default();
        let r = {
            let mut t = TOPO.lock();
            if at.mnt.ns_id() != self.id {
                Err(EINVAL)
            } else {
                crate::mount_tree::graft(&mut t, &mut g, &m, &at.mnt, &at.dentry)
            }
        };
        drop(g);
        r?;
        klog::event!(Info, "vfs", "mount", ns = self.id, mount = m.id, fs = m.sb.fstype, dev = m.sb.dev, ro = readonly);
        Ok(m)
    }
    /// The kernel's own unmount of the topmost mount at the mount root `p` (no busy check beyond submounts).
    /// EINVAL when `p` is not a mount root of this view or is the view's root, EBUSY while something is
    /// mounted below it.
    pub fn umount(&self, p: &Path) -> Result<(), Errno> {
        if !p.is_mount_root() || Arc::ptr_eq(&p.mnt, &self.root_mount()) {
            return Err(EINVAL);
        }
        let mut g = Grave::default();
        let r = {
            let mut t = TOPO.lock();
            if p.mnt.ns_id() != self.id || p.mnt.mountpoint().is_none() {
                Err(EINVAL)
            } else if !crate::mount_tree::children(&t, &p.mnt).is_empty() {
                Err(EBUSY)
            } else {
                crate::mount_tree::umount_tree(&mut t, &mut g, &p.mnt, true);
                Ok(())
            }
        };
        drop(g);
        r?;
        klog::event!(Info, "vfs", "umount", ns = self.id, mount = p.mnt.id, fs = p.mnt.sb.fstype);
        Ok(())
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
            match p.mnt.mountpoint() {
                Some((m, d)) => p = Path { mnt: m, dentry: d },
                None => return p,
            }
        }
        match p.dentry.parent() {
            Some(d) => cross_down(Path { mnt: p.mnt.clone(), dentry: d }),
            None => p,
        }
    }
    /// The absolute path of `p` seen from `root` as Linux's `d_path`/getcwd builds it. `None` when `p` is not
    /// reachable from `root` (another view's mount, a mount outside a chroot); the caller decides then.
    pub fn path_of(&self, root: &Path, p: &Path) -> Option<Vec<u8>> {
        path_of(root, p)
    }
    /// FS-10: what Linux's `d_path` shows for `p` seen from `root`: the path in this view, or, for a place
    /// `root` does not reach, the path up to the top of `p`'s own mount chain (linux-6.6/fs/d_path.c
    /// `__prepend_path` answers 1 or 2 and `d_path` keeps what it built): a file of another view (an fd opened
    /// before a container's pivot_root, SX-3c) shows its path in that view, a detached tree's file and the
    /// internal tmpfs of memfds (Linux's `shm_mnt`, `/memfd:<name>`) the path from their mount's root.
    /// Unlinked names get " (deleted)" as `d_path` adds it.
    pub fn d_path(&self, root: &Path, p: &Path) -> Vec<u8> {
        let mut out = match path_of(root, p) {
            Some(v) => v,
            None => path_to_top(p),
        };
        if p.dentry.is_unlinked() {
            out.extend_from_slice(b" (deleted)");
        }
        out
    }
    /// A copy of the view for the same owner (host tests; the kernel's unshare and clone call `copy_for`).
    #[cfg(test)]
    pub fn copy_view(&self) -> MountNs {
        self.copy_for(self.owner)
    }
    /// `copy_mnt_ns` (linux-6.6/fs/namespace.c:3744): a new view owned by `owner` with copies of every mount.
    /// For another owner shared mounts become slaves of their groups and the whole copy is locked
    /// (`CL_SHARED_TO_SLAVE`, `lock_mnt_tree`): its flags stay, its mounts cannot be unmounted one by one.
    pub fn copy_for(&self, owner: u32) -> MountNs {
        use crate::mount_tree::{CL_COPY_UNBINDABLE, CL_SHARED_TO_SLAVE};
        let id = NEXT_NS.fetch_add(1, Ordering::Relaxed);
        let other = owner != self.owner;
        let mut g = Grave::default();
        let (root, map) = {
            let mut t = TOPO.lock();
            let old = self.root_mount();
            let cl = CL_COPY_UNBINDABLE | if other { CL_SHARED_TO_SLAVE } else { 0 };
            let (root, map) = crate::mount_tree::copy_tree_map(&mut t, &mut g, &old, &old.root, cl, id);
            if other {
                crate::mount_tree::lock_tree(&t, &root);
            }
            (root, map)
        };
        drop(g);
        let ns = Self::build(id, root, owner, Some(Source { ns: self.id, map }));
        klog::event!(Info, "vfs", "ns.copy", from = self.id, ns = id, owner = owner, mounts = ns.mounts().len());
        ns
    }
    /// The view this one was copied from.
    pub fn source_id(&self) -> Option<u64> {
        self.source.as_ref().map(|s| s.ns)
    }
    /// `p` moved into this view: the copy of its mount made when this view was copied (None: not one of the
    /// source's mounts, or that copy is gone).
    pub fn translate(&self, p: &Path) -> Option<Path> {
        let m = self.source.as_ref()?.map.get(&p.mnt.id)?.upgrade()?;
        (m.ns_id() == self.id).then(|| Path { mnt: m, dentry: p.dentry.clone() })
    }
}

impl Drop for MountNs {
    fn drop(&mut self) {
        // `put_mnt_ns`: every mount of the view goes (`umount_tree` of all, no propagation).
        let mut g = Grave::default();
        {
            let mut t = TOPO.lock();
            let mine = t.view(self.id);
            for m in mine.iter().rev() {
                crate::mount_tree::unhash(&mut t, &mut g, m);
            }
            t.owners.remove(&self.id);
            g.mounts.extend(mine);
        }
        if self.source.is_some() {
            COPIES.fetch_sub(1, Ordering::Relaxed);
        }
        drop(g);
    }
}

/// The mount covering `p`, if any.
pub(crate) fn covering(p: &Path) -> Option<Arc<Mount>> {
    if p.dentry.mounted.load(Ordering::Acquire) == 0 {
        return None;
    }
    // The weak copy is upgraded outside the lock; a mount being taken out at this moment is either still
    // found (as with the table's lock) or gone.
    let w = COVER.lock().get(&(p.mnt.id, p.dentry.id)).cloned();
    w.and_then(|w| w.upgrade())
}

/// Steps onto whatever is mounted on `p`, to the topmost.
pub(crate) fn cross_down(mut p: Path) -> Path {
    while let Some(m) = covering(&p) {
        p = Path { dentry: m.root.clone(), mnt: m };
    }
    p
}

pub(crate) fn join(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for n in parts {
        out.push(b'/');
        out.extend_from_slice(n);
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// See `MountNs::path_of`.
pub(crate) fn path_of(root: &Path, p: &Path) -> Option<Vec<u8>> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut cur = p.clone();
    loop {
        if cur.same(root) {
            break;
        }
        if cur.is_mount_root() {
            match cur.mnt.mountpoint() {
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
    parts.reverse();
    Some(join(&parts))
}

/// `p`'s path from the top of its mount chain: up through dentries and, at each mount root, to the mountpoint,
/// until a mount without one (another view's root, a detached tree, an internal mount). getcwd puts
/// "(unreachable)" in front of it (linux-6.6/fs/d_path.c:432).
pub(crate) fn path_to_top(p: &Path) -> Vec<u8> {
    let mut parts: Vec<Vec<u8>> = Vec::new();
    let mut cur = p.clone();
    loop {
        if cur.is_mount_root() {
            match cur.mnt.mountpoint() {
                Some((m, d)) => {
                    cur = Path { mnt: m, dentry: d };
                    continue;
                }
                None => break,
            }
        }
        match cur.dentry.parent() {
            Some(par) => {
                parts.push(cur.dentry.name());
                cur = Path { mnt: cur.mnt.clone(), dentry: par };
            }
            None => break,
        }
    }
    parts.reverse();
    join(&parts)
}

/// Linux `is_path_reachable`: (`mnt`, `d`) lies at or below `root` through mountpoints.
pub(crate) fn reachable(mnt: &Arc<Mount>, d: &Arc<Dentry>, root: &Path) -> bool {
    let (mut m, mut d) = (mnt.clone(), d.clone());
    while !Arc::ptr_eq(&m, &root.mnt) {
        match m.mountpoint() {
            Some((pm, pd)) => {
                m = pm;
                d = pd;
            }
            None => return false,
        }
    }
    d.is_descendant_of(&root.dentry)
}
