//! Super blocks and dentries (the name cache).
//!
//! A dentry is one name of an inode: (parent, name) -> inode. For tmpfs the dentry tree *is* the directory
//! content (as Linux's ramfs/tmpfs: lookups that miss the cache are negative); a disk file system later fills
//! the cache from `Data::External` lookups. Children are kept by strong references, parents by strong
//! references upward; the cycle is broken when a name is removed and when the last mount of a file system
//! goes away (`prune`). Dropping a long chain of removed directories is iterative (`Drop`), so a hostile
//! depth cannot overflow the kernel stack.
//!
//! Locks: `vfs.sb.ns` (Mutex, per file system) serialises every change of names on that file system (create,
//! link, unlink, rename); `vfs.dentry.dir` (one directory's child map) and `vfs.dentry.link` (one dentry's name
//! and parent) are short spin sections and are never nested with each other's own class.

use crate::inode::{Cred, Data, Inode};
use crate::Errno;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ksync::{Mutex, MutexGuard, SpinLock, WaitQueue};
use uapi::errno::*;

static NEXT_DEV_MINOR: AtomicU32 = AtomicU32::new(1);

/// An anonymous device number (0:n) for a pseudo file system without a super block here (FS-3: pipefs,
/// anon_inodefs), from the same series as the mounted ones.
pub fn anon_dev() -> u64 {
    crate::abi::encode_dev(0, NEXT_DEV_MINOR.fetch_add(1, Ordering::Relaxed))
}
static NEXT_DENTRY: AtomicU64 = AtomicU64::new(1);

/// File systems whose names only the kernel makes (FS-2): programs cannot create, remove or rename there.
/// The errors follow Linux: in proc a missing name is ENOENT even for creating (its lookup fails), in sysfs
/// (kernfs) creating a file is EACCES and a directory or link EPERM; removing or renaming is EPERM in both,
/// after the usual permission checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fixed {
    Proc,
    Sys,
}

/// One mounted file system instance.
pub struct SuperBlock {
    /// `st_dev` (anonymous device 0:n like tmpfs).
    pub dev: u64,
    pub fstype: &'static str,
    /// Set for proc and sysfs: names come from the kernel only.
    pub fixed: Option<Fixed>,
    next_ino: AtomicU64,
    ns_lock: Mutex<()>,
    frozen: AtomicBool,
    /// Content changes under way (`write_begin` guards).
    writers: AtomicUsize,
    freeze_wq: WaitQueue,
    /// Mounts of this file system in any view; the tree is pruned when the last goes.
    pub(crate) mounts: AtomicUsize,
    /// What its regular files are charged to (FS-5: tmpfs instances; synthetic file systems have no files).
    pub pool: Arc<crate::pool::Pool>,
    /// A disk file system's side of the name operations (FS-7): None for tmpfs, proc, sysfs.
    pub ops: Option<Arc<dyn crate::fsops::FsOps>>,
    /// Serialises `FsOps::load_dir` (one directory filled at a time; `vfs.sb.load`).
    load_lock: Mutex<()>,
    /// The mount source /proc/<pid>/mounts shows (Linux `mnt_devname`): set by the mounter (a device path),
    /// else the type's name (AB-7a CR2).
    source: ksync::SpinLock<Option<alloc::string::String>>,
    /// AB-7c: the file system's own options as Linux's `show_options` prints them (each with a leading comma,
    /// e.g. ",mode=755,inode64"); empty for none. Set by the mounter.
    options: ksync::SpinLock<alloc::string::String>,
}

impl SuperBlock {
    pub fn new(fstype: &'static str) -> Arc<SuperBlock> {
        Self::with_fixed(fstype, None)
    }
    /// A synthetic file system (proc, sysfs).
    pub fn new_fixed(fstype: &'static str, fixed: Fixed) -> Arc<SuperBlock> {
        Self::with_fixed(fstype, Some(fixed))
    }
    /// A disk file system whose names live on its disk (`FsOps`).
    pub fn with_ops(fstype: &'static str, ops: Arc<dyn crate::fsops::FsOps>) -> Arc<SuperBlock> {
        Self::build(fstype, None, Some(ops))
    }
    fn with_fixed(fstype: &'static str, fixed: Option<Fixed>) -> Arc<SuperBlock> {
        Self::build(fstype, fixed, None)
    }
    fn build(fstype: &'static str, fixed: Option<Fixed>, ops: Option<Arc<dyn crate::fsops::FsOps>>) -> Arc<SuperBlock> {
        let minor = NEXT_DEV_MINOR.fetch_add(1, Ordering::Relaxed);
        Arc::new(SuperBlock {
            dev: crate::abi::encode_dev(0, minor),
            fstype,
            fixed,
            next_ino: AtomicU64::new(1),
            ns_lock: Mutex::named("vfs.sb.ns", ()),
            frozen: AtomicBool::new(false),
            writers: AtomicUsize::new(0),
            freeze_wq: WaitQueue::new(),
            mounts: AtomicUsize::new(0),
            pool: Arc::new(crate::pool::Pool::new(fstype)),
            ops,
            load_lock: Mutex::named("vfs.sb.load", ()),
            source: ksync::SpinLock::named("vfs.sb.source", None),
            options: ksync::SpinLock::named("vfs.sb.options", alloc::string::String::new()),
        })
    }
    /// The mount source (`mnt_devname`): what `set_source` gave, else the type's name.
    pub fn source(&self) -> alloc::string::String {
        self.source.lock().clone().unwrap_or_else(|| alloc::string::String::from(self.fstype))
    }
    pub fn set_source(&self, s: &str) {
        *self.source.lock() = Some(alloc::string::String::from(s));
    }
    /// The options /proc/<pid>/mounts and mountinfo show after the flags (`show_options`).
    pub fn options(&self) -> alloc::string::String {
        self.options.lock().clone()
    }
    pub fn set_options(&self, s: &str) {
        *self.options.lock() = alloc::string::String::from(s);
    }
    /// The key a name is stored and looked up by in this file system's directories.
    pub fn key(&self, name: &[u8]) -> Vec<u8> {
        match &self.ops {
            Some(o) => o.fold(name),
            None => name.to_vec(),
        }
    }
    pub fn next_ino(&self) -> u64 {
        self.next_ino.fetch_add(1, Ordering::Relaxed)
    }
    /// Held while names on this file system change.
    pub fn lock_names(&self) -> MutexGuard<'_, ()> {
        self.ns_lock.lock()
    }
    pub fn is_frozen(&self) -> bool {
        self.frozen.load(Ordering::SeqCst)
    }
    /// B1: every change of file content takes this guard for the duration of the change. While the file
    /// system is frozen it waits (in 100 ms slices, a `vfs freeze.writer_waits` event after 10 s) and answers
    /// EINTR when the calling program was killed meanwhile (Linux waits uninterruptibly here; B4 wants an end).
    pub fn write_begin(&self) -> Result<WriteGuard<'_>, Errno> {
        let mut slices = 0u64;
        loop {
            self.wait_thawed_counting(&mut slices)?;
            // Dekker pair with `freeze`: announce the writer, then look at the flag (both SeqCst).
            self.writers.fetch_add(1, Ordering::SeqCst);
            if !self.frozen.load(Ordering::SeqCst) {
                return Ok(WriteGuard(self));
            }
            // A freeze started meanwhile: step back so it can drain, then wait for the thaw.
            self.write_end();
        }
    }
    /// Waits until the file system is not frozen, without taking a guard (callers that are about to take
    /// another lock first wait here, so they do not wait while holding it).
    pub fn wait_thawed(&self) -> Result<(), Errno> {
        self.wait_thawed_counting(&mut 0)
    }
    fn wait_thawed_counting(&self, slices: &mut u64) -> Result<(), Errno> {
        while self.is_frozen() {
            if interrupted() {
                klog::event!(Info, "vfs", "freeze.writer_interrupted", fs = self.fstype, dev = self.dev, waited_ms = *slices * 100);
                return Err(EINTR);
            }
            // The kill check stays outside the condition: conditions run while the thread is about to block
            // and must be cheap; a kill is seen within one slice.
            if !self.freeze_wq.wait_until_timeout(|| !self.is_frozen(), FREEZE_SLICE_NS) {
                *slices += 1;
                if *slices == 100 {
                    klog::event!(Warn, "vfs", "freeze.writer_waits", fs = self.fstype, dev = self.dev, seconds = 10);
                }
            }
        }
        Ok(())
    }
    fn write_end(&self) {
        if self.writers.fetch_sub(1, Ordering::SeqCst) == 1 && self.frozen.load(Ordering::SeqCst) {
            self.freeze_wq.wake_all();
        }
    }
    /// Content changes under way (B1 freeze waits for 0).
    pub fn writers(&self) -> usize {
        self.writers.load(Ordering::SeqCst)
    }
}

/// One content change in progress (see `SuperBlock::write_begin`).
pub struct WriteGuard<'a>(&'a SuperBlock);

impl Drop for WriteGuard<'_> {
    fn drop(&mut self) {
        self.0.write_end();
    }
}

/// How often a writer held back by a freeze looks for a kill.
const FREEZE_SLICE_NS: u64 = 100_000_000;
/// How long `freeze` waits for changes already under way (tmpfs changes are memory copies: generous).
const FREEZE_DRAIN_NS: u64 = 10_000_000_000;

static INTERRUPT_CHECK: ksync::Once<fn() -> bool> = ksync::Once::new();
static SIGNAL_CHECK: ksync::Once<fn() -> bool> = ksync::Once::new();

/// The process layer tells the VFS how to see that the calling thread's program was killed (vfs-sys installs
/// it; kernel threads are never interrupted).
pub fn set_interrupt_check(f: fn() -> bool) {
    INTERRUPT_CHECK.get_or_init(|| f);
}

/// AB-6: the process layer's "a signal waits for this thread's handler" check (Linux `signal_pending`).
pub fn set_signal_check(f: fn() -> bool) {
    SIGNAL_CHECK.get_or_init(|| f);
}

/// Whether the calling program was killed OR (AB-6) has a signal to handle: blocking device reads
/// (pipe/FIFO/eventfd) ask it between their wait slices and end with EINTR, which the syscall exit restarts
/// per SA_RESTART (-ERESTARTSYS).
pub fn interrupted() -> bool {
    INTERRUPT_CHECK.get().is_some_and(|f| f()) || SIGNAL_CHECK.get().is_some_and(|f| f())
}

/// Whether the calling program was killed (the fatal-only check, for callers that must not restart).
pub fn killed() -> bool {
    INTERRUPT_CHECK.get().is_some_and(|f| f())
}

impl crate::FsFreeze for SuperBlock {
    /// Freezing holds new changes back and waits until the ones under way are done (quiescent); then the
    /// file system writes back and commits (`FsOps::freeze_fs`; tmpfs has nothing to write). If the changes do
    /// not end within 10 s the freeze is given up with EBUSY.
    fn freeze(&self) -> Result<(), Errno> {
        if self.frozen.swap(true, Ordering::SeqCst) {
            return Err(EBUSY);
        }
        if !self.freeze_wq.wait_until_timeout(|| self.writers.load(Ordering::SeqCst) == 0, FREEZE_DRAIN_NS) {
            let busy = self.writers();
            self.frozen.store(false, Ordering::SeqCst);
            self.freeze_wq.wake_all();
            klog::event!(Warn, "vfs", "freeze.busy", fs = self.fstype, dev = self.dev, writers = busy; "changes under way did not end; not frozen");
            return Err(EBUSY);
        }
        if let Some(ops) = &self.ops
            && let Err(e) = ops.freeze_fs()
        {
            self.frozen.store(false, Ordering::SeqCst);
            self.freeze_wq.wake_all();
            klog::event!(Warn, "vfs", "freeze.fs_failed", fs = self.fstype, dev = self.dev, errno = e; "the file system could not freeze; not frozen");
            return Err(e);
        }
        klog::event!(Info, "vfs", "freeze", fs = self.fstype, dev = self.dev);
        Ok(())
    }
    fn thaw(&self) {
        if self.is_frozen()
            && let Some(ops) = &self.ops
        {
            ops.thaw_fs();
        }
        if self.frozen.swap(false, Ordering::SeqCst) {
            klog::event!(Info, "vfs", "thaw", fs = self.fstype, dev = self.dev);
        }
        self.freeze_wq.wake_all();
    }
}

struct Link {
    name: Vec<u8>,
    parent: Option<Arc<Dentry>>,
    /// Position in the parent's listing (getdents cookies stay valid while others come and go).
    seq: u64,
    unlinked: bool,
}

#[derive(Default)]
struct DirMap {
    by_name: BTreeMap<Vec<u8>, Arc<Dentry>>,
    by_seq: BTreeMap<u64, Arc<Dentry>>,
    next_seq: u64,
    /// The directory was removed: nothing may be created in it any more (`IS_DEADDIR`).
    dead: bool,
}

pub struct Dentry {
    pub id: u64,
    pub sb: Arc<SuperBlock>,
    pub inode: Arc<Inode>,
    link: SpinLock<Link>,
    dir: SpinLock<DirMap>,
    /// Mounts on this dentry in any view (a hint; the view's table decides).
    pub(crate) mounted: AtomicU32,
    /// Its children are in the tree (always, except for directories of a file system with `FsOps` that were
    /// not looked into yet).
    loaded: AtomicBool,
    /// Its name's metadata charge (`DENTRY_BYTES` plus the name), given back with the dentry.
    charge: Option<crate::pool::MetaCharge>,
}

impl Dentry {
    pub fn new_root(sb: Arc<SuperBlock>, inode: Arc<Inode>) -> Arc<Dentry> {
        let d = Self::new(sb, inode, Vec::new(), None);
        d.mark_unloaded();
        d
    }
    /// A directory of a file system with `FsOps` starts without its children (`load`).
    fn mark_unloaded(&self) {
        if self.sb.ops.is_some() && self.is_dir() {
            self.loaded.store(false, Ordering::Release);
        }
    }
    /// Fills this directory from its file system once (`FsOps::load_dir`); a failed load is tried again next time.
    pub fn load(self: &Arc<Self>) -> Result<(), Errno> {
        if self.loaded.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(ops) = self.sb.ops.clone() else { return Ok(()) };
        let _g = self.sb.load_lock.lock();
        if self.loaded.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.dir.lock().dead {
            return Ok(());
        }
        ops.load_dir(self)?;
        self.loaded.store(true, Ordering::Release);
        Ok(())
    }
    /// `load` for callers that cannot report an error (the tree then shows what it has).
    fn load_quiet(self: &Arc<Self>) {
        if !self.loaded.load(Ordering::Acquire)
            && let Err(e) = self.load()
        {
            klog::event!(Warn, "vfs", "dir.load_failed", fs = self.sb.fstype, errno = e);
        }
    }
    /// A child the file system found on its disk (`FsOps::load_dir`): no hooks, no times touched.
    pub fn attach_loaded(self: &Arc<Self>, name: &[u8], inode: Arc<Inode>) -> Result<Arc<Dentry>, Errno> {
        let nc = self.name_charge(name)?;
        let d = Dentry::new_charged(self.sb.clone(), inode, name.to_vec(), Some(self.clone()), Some(nc));
        d.mark_unloaded();
        self.attach(&d)?;
        if d.is_dir() {
            self.inode.meta.inc_nlink();
        }
        Ok(d)
    }
    fn new(sb: Arc<SuperBlock>, inode: Arc<Inode>, name: Vec<u8>, parent: Option<Arc<Dentry>>) -> Arc<Dentry> {
        Self::new_charged(sb, inode, name, parent, None)
    }
    fn new_charged(sb: Arc<SuperBlock>, inode: Arc<Inode>, name: Vec<u8>, parent: Option<Arc<Dentry>>, charge: Option<crate::pool::MetaCharge>) -> Arc<Dentry> {
        Arc::new(Dentry {
            id: NEXT_DENTRY.fetch_add(1, Ordering::Relaxed),
            sb,
            inode,
            link: SpinLock::named("vfs.dentry.link", Link { name, parent, seq: 0, unlinked: false }),
            dir: SpinLock::named("vfs.dentry.dir", DirMap { next_seq: 1, ..Default::default() }),
            mounted: AtomicU32::new(0),
            loaded: AtomicBool::new(true),
            charge,
        })
    }
    /// The charge of a new name in this directory's file system (ENOSPC when its pool refuses).
    fn name_charge(&self, name: &[u8]) -> Result<crate::pool::MetaCharge, Errno> {
        self.sb.pool.charge_meta(crate::pool::DENTRY_BYTES + name.len() as u64)
    }
    /// The charge of a new inode with `data` in this directory's file system.
    fn inode_charge(&self, data: &Data) -> Result<crate::pool::MetaCharge, Errno> {
        let target = match data {
            Data::Symlink(t) => t.len() as u64,
            _ => 0,
        };
        self.sb.pool.charge_inode(crate::pool::INODE_BYTES + target)
    }
    /// A dentry for a name a synthetic directory computed (`synth::lookup`): it knows its parent and name for
    /// the walk and `getcwd`, but is not entered in the parent's child map (every lookup asks the source again).
    pub fn ephemeral(parent: &Arc<Dentry>, name: &[u8], inode: Arc<Inode>) -> Arc<Dentry> {
        Dentry::new(parent.sb.clone(), inode, name.to_vec(), Some(parent.clone()))
    }
    pub fn is_dir(&self) -> bool {
        self.inode.is_dir()
    }
    pub fn name(&self) -> Vec<u8> {
        self.link.lock().name.clone()
    }
    /// The parent inside the same file system (None for its root).
    pub fn parent(&self) -> Option<Arc<Dentry>> {
        self.link.lock().parent.clone()
    }
    /// Its name if it still sits at listing position `seq` (not renamed or removed since a listing picked it up):
    /// read together with the position, so a listing never shows a renamed entry's new name at its old place.
    pub fn name_at(&self, seq: u64) -> Option<Vec<u8>> {
        let l = self.link.lock();
        (l.seq == seq && !l.unlinked).then(|| l.name.clone())
    }
    pub fn is_unlinked(&self) -> bool {
        self.link.lock().unlinked
    }
    pub fn is_dead_dir(&self) -> bool {
        self.dir.lock().dead
    }
    pub fn child(self: &Arc<Self>, name: &[u8]) -> Option<Arc<Dentry>> {
        self.load_quiet();
        let key = self.sb.key(name);
        if let Some(d) = self.dir.lock().by_name.get(&key).cloned() {
            return Some(d);
        }
        // Another name the file system knows (FAT: the 8.3 alias), outside the map lock.
        let real = self.sb.ops.as_ref()?.alias(self, name)?;
        let key = self.sb.key(&real);
        self.dir.lock().by_name.get(&key).cloned()
    }
    pub fn child_count(self: &Arc<Self>) -> usize {
        self.load_quiet();
        self.dir.lock().by_name.len()
    }
    /// The children in the cache now, without loading the directory (INT-W7: fs-fat refreshes the owner and
    /// mode of a moved subtree; what is not cached yet is loaded later with the right ones).
    pub fn cached_children(&self) -> Vec<Arc<Dentry>> {
        self.dir.lock().by_name.values().cloned().collect()
    }
    /// Children in listing order starting at sequence number `from` (inclusive), at most `max`.
    pub fn children_from(self: &Arc<Self>, from: u64, max: usize) -> Vec<(u64, Arc<Dentry>)> {
        self.load_quiet();
        self.dir.lock().by_seq.range(from..).take(max).map(|(s, d)| (*s, d.clone())).collect()
    }
    /// stat of this dentry's inode.
    pub fn attr(&self) -> crate::abi::Attr {
        let entries = if self.is_dir() { self.dir.lock().by_name.len() } else { 0 };
        let mut a = self.inode.attr(self.sb.dev, entries);
        if self.is_dir()
            && let Some((size, blocks)) = self.sb.ops.as_ref().and_then(|o| o.dir_attr(&self.inode))
        {
            a.size = size;
            a.blocks = blocks;
        }
        a
    }
    /// Whether `self` is `other` or below it (same file system). Iterative: the depth may be anything.
    pub fn is_descendant_of(self: &Arc<Self>, other: &Arc<Dentry>) -> bool {
        let mut cur = self.clone();
        loop {
            if Arc::ptr_eq(&cur, other) {
                return true;
            }
            match cur.parent() {
                Some(p) => cur = p,
                None => return false,
            }
        }
    }

    /// Makes a new inode of `mode` named `name` in this directory. Caller holds `lock_names` and checked
    /// that the name is free and the directory alive.
    pub fn create_child(self: &Arc<Self>, name: &[u8], mode: u32, cred: Cred, data: Data) -> Result<Arc<Dentry>, Errno> {
        let (mode, cred) = self.init_owner(mode, cred);
        if let Some(ops) = self.sb.ops.clone() {
            // A disk file system makes the node on its disk first (its rules decide there); `data` is tmpfs's.
            self.load()?;
            let nc = self.name_charge(name)?;
            let inode = ops.create_node(self, name, mode, cred, &data)?;
            let d = Dentry::new_charged(self.sb.clone(), inode, name.to_vec(), Some(self.clone()), Some(nc));
            self.attach(&d)?;
            if d.is_dir() {
                self.inode.meta.inc_nlink();
            }
            self.inode.meta.touch_mtime();
            crate::inotify::hooks::created(&self.inode, &d);
            return Ok(d);
        }
        // Metadata is charged before anything is allocated (review INT-A1d fs 1): a full pool is ENOSPC here,
        // never an exhausted kernel heap.
        let ic = self.inode_charge(&data)?;
        let nc = self.name_charge(name)?;
        let inode = Inode::with_charge(self.sb.next_ino(), mode, cred, data, Some(ic));
        let d = Dentry::new_charged(self.sb.clone(), inode, name.to_vec(), Some(self.clone()), Some(nc));
        self.attach(&d)?;
        if d.is_dir() {
            self.inode.meta.inc_nlink();
        }
        self.inode.meta.touch_mtime();
        crate::inotify::hooks::created(&self.inode, &d);
        Ok(d)
    }
    /// O_TMPFILE: a regular inode below this directory without a name in it (nlink 0, already unlinked; it
    /// goes away with its last open file unless `linkable` lets linkat give it a name).
    /// Owner and mode of a new node in this directory: a set-group-ID directory gives its group, and its
    /// S_ISGID to new directories (`inode_init_owner`, linux-6.6/fs/inode.c:2370); a new group-executable
    /// file keeps S_ISGID only when the creator is in that group or root (`mode_strip_sgid`, inode.c:2582).
    pub fn init_owner(&self, mut mode: u32, mut cred: Cred) -> (u32, Cred) {
        const S_IXGRP: u32 = 0o010;
        let dir_mode = self.inode.meta.mode();
        if dir_mode & crate::abi::S_ISGID == 0 {
            return (mode, cred);
        }
        let gid = self.inode.meta.gid();
        if mode & uapi::S_IFMT == uapi::S_IFDIR {
            mode |= crate::abi::S_ISGID;
        } else if mode & (crate::abi::S_ISGID | S_IXGRP) == crate::abi::S_ISGID | S_IXGRP && cred.uid != 0 && !crate::inode::in_group(cred, gid) {
            mode &= !crate::abi::S_ISGID;
        }
        cred.gid = gid;
        (mode, cred)
    }

    pub fn tmpfile_child(self: &Arc<Self>, mode: u32, cred: Cred, data: Data, linkable: bool) -> Result<Arc<Dentry>, Errno> {
        let (mode, cred) = self.init_owner(mode, cred);
        if self.sb.ops.is_some() {
            // vfat has no tmpfile method (`vfs_tmpfile`).
            return Err(uapi::errno::EOPNOTSUPP);
        }
        let ic = self.inode_charge(&data)?;
        let inode = Inode::with_charge(self.sb.next_ino(), mode, cred, data, Some(ic));
        inode.meta.clear_nlink();
        inode.linkable.store(linkable, Ordering::Release);
        let name = alloc::format!("#{}", inode.meta.ino).into_bytes();
        let d = Dentry::new(self.sb.clone(), inode, name, Some(self.clone()));
        d.link.lock().unlinked = true;
        Ok(d)
    }
    /// FS-10: a nameless inode below this directory that shows as `name` in paths (Linux's pseudo files,
    /// `alloc_file_pseudo`: a memfd's `memfd:<name>`): nlink 0, already unlinked, never linkable.
    pub fn nameless_child(self: &Arc<Self>, name: Vec<u8>, mode: u32, cred: Cred, data: Data) -> Result<Arc<Dentry>, Errno> {
        let ic = self.inode_charge(&data)?;
        let inode = Inode::with_charge(self.sb.next_ino(), mode, cred, data, Some(ic));
        inode.meta.clear_nlink();
        let d = Dentry::new(self.sb.clone(), inode, name, Some(self.clone()));
        d.link.lock().unlinked = true;
        Ok(d)
    }
    /// A new name for an existing inode (hard link).
    pub fn link_child(self: &Arc<Self>, name: &[u8], inode: Arc<Inode>) -> Result<Arc<Dentry>, Errno> {
        if let Some(ops) = &self.sb.ops {
            ops.link(self, name, &inode)?;
        }
        let nc = self.name_charge(name)?;
        let d = Dentry::new_charged(self.sb.clone(), inode, name.to_vec(), Some(self.clone()), Some(nc));
        self.attach(&d)?;
        d.inode.add_link();
        d.inode.meta.touch_ctime();
        self.inode.meta.touch_mtime();
        crate::inotify::hooks::linked(&self.inode, &d.inode, name);
        Ok(d)
    }
    fn attach(&self, d: &Arc<Dentry>) -> Result<(), Errno> {
        let name = self.sb.key(&d.name());
        let seq = {
            let mut m = self.dir.lock();
            if m.dead {
                return Err(ENOENT);
            }
            if m.by_name.contains_key(&name) {
                return Err(EEXIST);
            }
            let seq = m.next_seq;
            m.next_seq += 1;
            m.by_name.insert(name, d.clone());
            m.by_seq.insert(seq, d.clone());
            seq
        };
        d.link.lock().seq = seq;
        Ok(())
    }
    /// Removes the name `child` from this directory (unlink/rmdir/rename-over). Returns the removed dentry.
    pub fn detach(&self, name: &[u8]) -> Option<Arc<Dentry>> {
        let key = self.sb.key(name);
        let d = {
            let mut m = self.dir.lock();
            let d = m.by_name.remove(&key)?;
            let seq = d.link.lock().seq;
            m.by_seq.remove(&seq);
            d
        };
        d.link.lock().unlinked = true;
        Some(d)
    }
    /// Marks a removed directory dead (nothing can be created in it, lookups find nothing).
    pub fn mark_dead(&self) {
        self.dir.lock().dead = true;
    }
    /// Moves `d` (already detached from its old parent) under `self` as `name`, replacing nothing (the
    /// caller detached a target first).
    pub fn reattach(self: &Arc<Self>, d: &Arc<Dentry>, name: &[u8]) -> Result<(), Errno> {
        let key = self.sb.key(name);
        let seq = {
            let mut m = self.dir.lock();
            if m.dead {
                return Err(ENOENT);
            }
            if m.by_name.contains_key(&key) {
                return Err(EEXIST);
            }
            let seq = m.next_seq;
            m.next_seq += 1;
            m.by_name.insert(key, d.clone());
            m.by_seq.insert(seq, d.clone());
            seq
        };
        let old_parent = {
            let mut l = d.link.lock();
            l.name = name.to_vec();
            l.seq = seq;
            l.unlinked = false;
            l.parent.replace(self.clone())
        };
        drop(old_parent);
        Ok(())
    }
    /// Swaps the names of two dentries (RENAME_EXCHANGE). Caller holds `lock_names`.
    pub fn exchange(a_dir: &Arc<Dentry>, a_name: &[u8], b_dir: &Arc<Dentry>, b_name: &[u8]) -> Result<(), Errno> {
        if Arc::ptr_eq(a_dir, b_dir) {
            let (ka, kb) = (a_dir.sb.key(a_name), a_dir.sb.key(b_name));
            let (a, b) = {
                let mut m = a_dir.dir.lock();
                let a = m.by_name.get(&ka).cloned().ok_or(ENOENT)?;
                let b = m.by_name.get(&kb).cloned().ok_or(ENOENT)?;
                m.by_name.insert(ka, b.clone());
                m.by_name.insert(kb, a.clone());
                // One link lock at a time (same lock class: never nested).
                let sa = a.link.lock().seq;
                let sb = b.link.lock().seq;
                m.by_seq.insert(sa, b.clone());
                m.by_seq.insert(sb, a.clone());
                (a, b)
            };
            let sa = {
                let mut l = a.link.lock();
                l.name = b_name.to_vec();
                l.seq
            };
            let sb = {
                let mut l = b.link.lock();
                l.name = a_name.to_vec();
                core::mem::replace(&mut l.seq, sa)
            };
            a.link.lock().seq = sb;
            return Ok(());
        }
        let a = a_dir.detach(a_name).ok_or(ENOENT)?;
        let b = match b_dir.detach(b_name) {
            Some(b) => b,
            None => {
                let _ = a_dir.reattach(&a, a_name);
                return Err(ENOENT);
            }
        };
        b_dir.reattach(&a, b_name)?;
        a_dir.reattach(&b, a_name)?;
        if a.is_dir() != b.is_dir() {
            // Each directory's ".." moved: the link counts of the parents follow.
            let (to_inc, to_dec) = if a.is_dir() { (b_dir, a_dir) } else { (a_dir, b_dir) };
            to_inc.inode.meta.inc_nlink();
            to_dec.inode.meta.dec_nlink();
        }
        Ok(())
    }

    /// The path of this dentry from its file system's root (bytes, "/a/b"; "/" for the root).
    pub fn path_in_fs(self: &Arc<Self>) -> Vec<Vec<u8>> {
        let mut names = Vec::new();
        let mut cur = self.clone();
        while let Some(p) = cur.parent() {
            names.push(cur.name());
            cur = p;
        }
        names.reverse();
        names
    }

    /// Breaks every parent<->child cycle below `root` (the file system's last mount went away). Iterative.
    pub fn prune(root: &Arc<Dentry>) {
        let mut work = alloc::vec![root.clone()];
        let mut all = Vec::new();
        while let Some(d) = work.pop() {
            let kids = {
                let mut m = d.dir.lock();
                m.by_seq.clear();
                core::mem::take(&mut m.by_name)
            };
            work.extend(kids.into_values());
            all.push(d);
        }
        // Dropped deepest-first is not needed: `Drop` unwinds parent chains without recursion.
        drop(all);
    }
}

impl Drop for Dentry {
    fn drop(&mut self) {
        // FS-14: a removed name's last reference: its inode's IN_DELETE_SELF if that was its last name.
        if self.link.get_mut().unlinked {
            crate::inotify::hooks::dentry_gone(&self.inode);
        }
        // Unwind the parent chain by hand: a parent whose last reference was ours is dropped here after its
        // own parent was taken out, so the drop never recurses.
        let mut next = self.link.get_mut().parent.take();
        while let Some(p) = next {
            match Arc::try_unwrap(p) {
                Ok(mut d) => next = d.link.get_mut().parent.take(),
                Err(_) => break,
            }
        }
    }
}
