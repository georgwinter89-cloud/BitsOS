//! Open files and per-process fd tables (owner FS).
//!
//! `OpenFile` is Linux's `struct file`: the object, its status flags (F_GETFL/F_SETFL), its position and, when
//! it was opened by name, the place it was found (`Path`, for fstat, getdents64, fchdir and *at calls).
//! `FdTable` maps numbers to open files with a close-on-exec bit per number; it grows on demand up to the
//! process's RLIMIT_NOFILE (default 1024, ceiling `NR_OPEN` like Linux: R1 says this is the only limit).
//! The file-system state (cwd, root, umask; Linux's `fs_struct`) is `FsState`, apart from the table (FS-16).

use crate::abi::{self, Attr};
use crate::inode::{Data, Inode};
use crate::mount_ns::Path;
use crate::node::File;
use crate::Errno;
use crate::epoll::EpItem;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use ksync::{Mutex, SpinLock};
use mm_core::grace::{self, Retire};
use mm_core::rcu_index::RcuIndex;
use uapi::errno::*;

#[derive(Clone, Copy, Debug, Default)]
pub struct OpenFlags {
    pub read: bool,
    pub write: bool,
    pub append: bool,
    /// Taken as the close-on-exec bit of the fd when installed with `install_open`.
    pub cloexec: bool,
    pub nonblock: bool,
}

impl OpenFlags {
    pub fn from_linux(f: u64) -> Self {
        let acc = f & abi::O_ACCMODE;
        let path = f & abi::O_PATH != 0;
        OpenFlags {
            read: !path && (acc == abi::O_RDONLY || acc == abi::O_RDWR),
            write: !path && (acc == abi::O_WRONLY || acc == abi::O_RDWR),
            append: f & abi::O_APPEND != 0,
            cloexec: f & abi::O_CLOEXEC != 0,
            nonblock: f & abi::O_NONBLOCK != 0,
        }
    }
    fn to_linux(self) -> u64 {
        let acc = match (self.read, self.write) {
            (true, true) => abi::O_RDWR,
            (false, true) => abi::O_WRONLY,
            _ => abi::O_RDONLY,
        };
        acc | if self.append { abi::O_APPEND } else { 0 } | if self.nonblock { abi::O_NONBLOCK } else { 0 }
    }
}

pub struct OpenFile {
    pub file: Arc<dyn File>,
    /// Access as opened (read/write never change; append/nonblock: see `status_flags`).
    pub flags: OpenFlags,
    status: AtomicU64,
    /// F_GETFL shows O_LARGEFILE (every open by name on 64-bit Linux); pseudo files (pipes, eventfd,
    /// sockets) never have it.
    largefile: bool,
    pos: Mutex<u64>,
    /// The position as of the last unlock, for readers that must not take `pos` (fdinfo; Linux reads
    /// `f_pos` unlocked there). AB-7a CR3.
    pos_mirror: AtomicU64,
    /// Where it was opened (None for objects without a name: captures, pipes, sockets).
    pub path: Option<Path>,
    /// FS-4: the epoll items watching this open file description (Linux `file->f_ep`); they leave their epolls
    /// when it goes (`Drop`).
    ep: SpinLock<Vec<Weak<EpItem>>>,
    /// An open for writing of a regular file holds its inode's write access until the last close (ETXTBSY).
    _write: Option<crate::inode::WriteAccess>,
}

impl OpenFile {
    pub fn new(file: Arc<dyn File>, flags: OpenFlags) -> Arc<Self> {
        Arc::new(OpenFile { file, flags, status: AtomicU64::new(flags.to_linux()), largefile: true, pos: Mutex::named("vfs.file.pos", 0), pos_mirror: AtomicU64::new(0), path: None, ep: SpinLock::named("vfs.file.ep", Vec::new()), _write: None })
    }
    /// FS-3: a pseudo file without a name (`alloc_file_pseudo`: pipe ends, eventfd): `status` is its whole
    /// F_GETFL (access mode and status flags), without O_LARGEFILE.
    pub fn anon(file: Arc<dyn File>, status: u64) -> Arc<Self> {
        let flags = OpenFlags::from_linux(status);
        Arc::new(OpenFile { file, flags, status: AtomicU64::new(status & !abi::OPEN_ONLY), largefile: false, pos: Mutex::named("vfs.file.pos", 0), pos_mirror: AtomicU64::new(0), path: None, ep: SpinLock::named("vfs.file.ep", Vec::new()), _write: None })
    }
    /// `with_path` holding the inode's write access (an open for writing of a regular file).
    pub fn with_path_writing(file: Arc<dyn File>, status: u64, path: Path, write: Option<crate::inode::WriteAccess>) -> Arc<Self> {
        let flags = OpenFlags::from_linux(status);
        let status = status & !abi::OPEN_ONLY;
        Arc::new(OpenFile { file, flags, status: AtomicU64::new(status), largefile: true, pos: Mutex::named("vfs.file.pos", 0), pos_mirror: AtomicU64::new(0), path: Some(path), ep: SpinLock::named("vfs.file.ep", Vec::new()), _write: write })
    }
    /// An open by name: `status` are the open flags without the open-only bits.
    pub fn with_path(file: Arc<dyn File>, status: u64, path: Path) -> Arc<Self> {
        let flags = OpenFlags::from_linux(status);
        let status = status & !abi::OPEN_ONLY;
        Arc::new(OpenFile { file, flags, status: AtomicU64::new(status), largefile: true, pos: Mutex::named("vfs.file.pos", 0), pos_mirror: AtomicU64::new(0), path: Some(path), ep: SpinLock::named("vfs.file.ep", Vec::new()), _write: None })
    }
    /// F_GETFL: access mode and status flags. 64-bit Linux adds O_LARGEFILE at every open
    /// (`force_o_largefile`), but `build_open_flags` strips it again for O_PATH.
    pub fn status_flags(&self) -> u64 {
        let s = self.status.load(Ordering::Relaxed);
        if s & abi::O_PATH != 0 || !self.largefile { s } else { s | abi::O_LARGEFILE }
    }
    /// F_SETFL: only `SETFL_MASK` bits change.
    pub fn set_status_flags(&self, v: u64) {
        let mut s = self.status.load(Ordering::Relaxed);
        while let Err(now) = self.status.compare_exchange_weak(s, (s & !abi::SETFL_MASK) | (v & abi::SETFL_MASK), Ordering::Relaxed, Ordering::Relaxed) {
            s = now;
        }
    }
    /// FS-4 (`attach_epitem`): an epoll item now watches this open file.
    pub(crate) fn link_ep(&self, it: Weak<EpItem>) -> Result<(), Errno> {
        let mut l = self.ep.lock();
        // Links of items that are gone are dropped here, so the list never grows past the live items.
        l.retain(|w| w.strong_count() > 0);
        l.try_reserve(1).map_err(|_| ENOMEM)?;
        l.push(it);
        Ok(())
    }
    /// FS-4: the item no longer watches this open file (EPOLL_CTL_DEL, the epoll's end).
    pub(crate) fn unlink_ep(&self, it: &Arc<EpItem>) {
        let me = Arc::downgrade(it);
        let gone: Vec<Weak<EpItem>> = {
            let mut l = self.ep.lock();
            let mut gone = Vec::new();
            if let Some(i) = l.iter().position(|w| w.ptr_eq(&me)) {
                gone.push(l.remove(i));
            }
            gone
        };
        drop(gone);
    }
    /// FS-4: the epoll items watching this open file now (`file->f_ep`).
    pub(crate) fn ep_links(&self) -> Vec<Weak<EpItem>> {
        self.ep.lock().clone()
    }
    /// FS-4: whether no epoll watches this open file.
    pub(crate) fn ep_links_empty(&self) -> bool {
        self.ep.lock().iter().all(|w| w.strong_count() == 0)
    }
    pub fn is_path_only(&self) -> bool {
        self.status.load(Ordering::Relaxed) & abi::O_PATH != 0
    }
    pub fn appending(&self) -> bool {
        self.status.load(Ordering::Relaxed) & abi::O_APPEND != 0
    }
    pub fn nonblocking(&self) -> bool {
        self.status.load(Ordering::Relaxed) & abi::O_NONBLOCK != 0
    }
    pub fn can_read(&self) -> bool {
        self.flags.read
    }
    pub fn can_write(&self) -> bool {
        self.flags.write
    }
    /// The inode behind it when it was opened by name.
    pub fn inode(&self) -> Option<&Arc<Inode>> {
        self.path.as_ref().map(|p| &p.dentry.inode)
    }
    fn data_inode(&self) -> Option<&Arc<Inode>> {
        self.inode().filter(|i| matches!(i.data, Data::Reg(_) | Data::External(_)))
    }
    pub fn is_dir(&self) -> bool {
        self.inode().is_some_and(|i| i.is_dir())
    }

    pub fn read(&self, buf: &mut [u8]) -> Result<usize, Errno> {
        if !self.flags.read {
            return Err(EBADF);
        }
        if !self.file.seekable() {
            return self.file.read_stream(buf, self.nonblocking());
        }
        let mut p = self.pos_guard();
        let n = self.file.read_at(*p, buf)?;
        *p += n as u64;
        Ok(n)
    }
    pub fn write(&self, buf: &[u8]) -> Result<usize, Errno> {
        if !self.flags.write {
            return Err(EBADF);
        }
        if !self.file.seekable() {
            return self.file.write_stream(buf, self.nonblocking());
        }
        let mut p = self.pos_guard();
        self.write_locked(&mut p, buf)
    }
    /// A write at the locked position `p` (O_APPEND: at the end, atomically for tmpfs files).
    pub fn write_locked(&self, p: &mut u64, buf: &[u8]) -> Result<usize, Errno> {
        if self.appending() {
            if let (Some(i), Some(path)) = (self.data_inode(), self.path.as_ref()) {
                let _g = path.dentry.sb.write_begin()?;
                let (n, end) = i.append(buf)?;
                *p = end;
                return Ok(n);
            }
            *p = self.file.size();
        }
        let n = self.file.write_at(*p, buf)?;
        *p += n as u64;
        Ok(n)
    }
    /// FS-10: how much of a write of `count` bytes at `pos` (O_APPEND: at the end) a sealed tmpfs file takes,
    /// decided once for the whole request as Linux's single `generic_perform_write` does (the syscall layer
    /// copies in chunks; each chunk is checked again). EPERM when the seals refuse the first step.
    pub fn seal_clip(&self, pos: u64, count: u64) -> Result<u64, Errno> {
        if count == 0 {
            return Ok(0);
        }
        let Some(i) = self.data_inode() else { return Ok(count) };
        let Some(c) = i.page_cache() else { return Ok(count) };
        match c.write_limit(if self.appending() { None } else { Some(pos) }, count.min(usize::MAX as u64) as usize) {
            Ok(n) => Ok(n as u64),
            Err(e) => {
                crate::memfd::seal_refused(crate::memfd::Refused::Write, i.meta.ino, c.seals().unwrap_or(0), e);
                Err(e)
            }
        }
    }
    /// pread64: the position stays.
    pub fn pread(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if !self.flags.read {
            return Err(EBADF);
        }
        if !self.file.seekable() {
            return Err(ESPIPE);
        }
        self.file.read_at(pos, buf)
    }
    /// pwrite64: the position stays; with O_APPEND the data still goes to the end (Linux's documented
    /// behaviour).
    pub fn pwrite(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        if !self.flags.write {
            return Err(EBADF);
        }
        if !self.file.seekable() {
            return Err(ESPIPE);
        }
        if self.appending() {
            if let (Some(i), Some(path)) = (self.data_inode(), self.path.as_ref()) {
                let _g = path.dentry.sb.write_begin()?;
                return i.append(buf).map(|(n, _)| n);
            }
        }
        self.file.write_at(pos, buf)
    }
    /// FS-1b pwritev2 with RWF_APPEND: at the end whatever the open flags say; returns (written, new end).
    pub fn pwrite_end(&self, buf: &[u8]) -> Result<(usize, u64), Errno> {
        if !self.flags.write {
            return Err(EBADF);
        }
        if !self.file.seekable() {
            return Err(ESPIPE);
        }
        if let (Some(i), Some(path)) = (self.data_inode(), self.path.as_ref()) {
            let _g = path.dentry.sb.write_begin()?;
            return i.append(buf);
        }
        let end = self.file.size();
        self.file.write_at(end, buf).map(|n| (n, end + n as u64))
    }
    /// lseek (SEEK_SET/CUR/END/DATA/HOLE). Directories and proc files take only SET and CUR.
    pub fn seek(&self, off: i64, whence: u32) -> Result<u64, Errno> {
        if let Some(r) = self.file.lseek(off, whence) {
            if let Ok(v) = r {
                *self.pos_guard() = v;
            }
            return r;
        }
        if !self.file.seekable() {
            return Err(ESPIPE);
        }
        let mut p = self.pos_guard();
        let cur = *p as i64;
        let data = self.data_inode();
        // Regular data: VFS files, or nameless objects that say they are regular. Directories and proc files
        // (seq files on Linux) take only SET and CUR.
        let regular = data.is_some() || (self.path.is_none() && self.file.mode() & uapi::S_IFMT == uapi::S_IFREG);
        let new: i64 = match whence {
            abi::SEEK_SET => off,
            abi::SEEK_CUR => cur.checked_add(off).ok_or(EINVAL)?,
            abi::SEEK_END if regular => (self.file.size() as i64).checked_add(off).ok_or(EINVAL)?,
            abi::SEEK_DATA | abi::SEEK_HOLE if regular => {
                let hole = whence == abi::SEEK_HOLE;
                if off < 0 {
                    return Err(ENXIO);
                }
                let at = match data {
                    Some(i) => i.seek_data(off as u64, hole),
                    None => {
                        let size = self.file.size();
                        ((off as u64) < size).then_some(if hole { size } else { off as u64 })
                    }
                };
                at.ok_or(ENXIO)? as i64
            }
            _ => return Err(EINVAL),
        };
        if new < 0 {
            return Err(EINVAL);
        }
        *p = new as u64;
        Ok(*p)
    }
    pub fn pos(&self) -> u64 {
        *self.pos_guard()
    }
    /// The position without its lock (the value at the last unlock; it may change at once). For fdinfo,
    /// whose own read holds a position lock of the same class (AB-7a CR3).
    pub fn pos_racy(&self) -> u64 {
        self.pos_mirror.load(Ordering::Relaxed)
    }
    fn pos_guard(&self) -> PosGuard<'_> {
        PosGuard { g: self.pos.lock(), mirror: &self.pos_mirror }
    }
    /// Runs `f` with the position locked (getdents64 reads and advances it in one step).
    pub fn with_pos<R>(&self, f: impl FnOnce(&mut u64) -> R) -> R {
        let mut p = self.pos_guard();
        f(&mut p)
    }
    /// fstat: the inode's stat when opened by name, else what the object says about itself.
    pub fn stat(&self) -> Attr {
        match &self.path {
            Some(p) => p.attr(),
            None => self.file.attr().unwrap_or(Attr {
                mode: self.file.mode(),
                size: self.file.size(),
                nlink: 1,
                blksize: 4096,
                uid: 1000,
                gid: 1000,
                ..Default::default()
            }),
        }
    }
}

impl Drop for OpenFile {
    /// FS-4 (`eventpoll_release_file`): the epoll items watching this open file description leave their epolls.
    fn drop(&mut self) {
        // FS-14: `fsnotify_close` comes first in `__fput`.
        use crate::inotify::{hooks, IN_CLOSE_NOWRITE, IN_CLOSE_WRITE};
        hooks::file(self, if self.can_write() { IN_CLOSE_WRITE } else { IN_CLOSE_NOWRITE });
        let links = core::mem::take(self.ep.get_mut());
        if !links.is_empty() {
            crate::epoll::release_file(links);
        }
    }
}

/// A thread's file-system state (Linux `fs_struct`, linux-6.6/include/linux/fs_struct.h): working directory, root
/// and umask. FS-16: it lives apart from the fd table, as in Linux (kernel/fork.c `copy_fs`/`copy_files`): the
/// process layer holds it behind an `Arc` per thread, CLONE_FS shares it, a clone without CLONE_FS and
/// unshare(CLONE_FS) take a `copy` (`copy_fs_struct`); CLONE_FILES shares only the fd table.
pub struct FsState {
    cwd: SpinLock<Option<Path>>,
    /// chroot(2)'s root (None = the view's root).
    root: SpinLock<Option<Path>>,
    umask: AtomicU32,
    /// SX-3a: the mount view its cwd and root were last placed in (0: not yet). A state entering a view copied
    /// from that one has its places moved onto the copies (`enter`), as `copy_mnt_ns` moves the copier's.
    view: AtomicU64,
}

impl FsState {
    pub fn new() -> Self {
        FsState { cwd: SpinLock::named("vfs.fs.cwd", None), root: SpinLock::named("vfs.fs.root", None), umask: AtomicU32::new(0o022), view: AtomicU64::new(0) }
    }
    /// `copy_fs_struct`: the same cwd, root and umask, from now on independent.
    pub fn copy(&self) -> Self {
        let t = FsState::new();
        *t.cwd.lock() = self.cwd();
        *t.root.lock() = self.root();
        t.set_umask(self.umask());
        t.view.store(self.view.load(Ordering::Acquire), Ordering::Release);
        t
    }
    /// SX-3a: this state is used in view `ns`. Coming from the view `ns` was copied from (a clone with
    /// CLONE_NEWNS, another thread's unshare), its cwd and root move onto the copies of their mounts; a state
    /// never placed takes `ns` as its view. A state already in a copy keeps its places when used with the view
    /// it came from (a caller that still passes the boot view).
    pub fn enter(&self, ns: &crate::MountNs) {
        let was = self.view.load(Ordering::Acquire);
        if was == ns.id {
            return;
        }
        if ns.source_id() == Some(was) {
            self.move_into(ns);
        } else if was == 0 {
            self.view.store(ns.id, Ordering::Release);
        }
    }
    /// `copy_mnt_ns`'s second pass for this state: cwd and root onto the copies in `ns` (places on mounts
    /// `ns` has no copy of stay), then `ns` is its view.
    pub fn move_into(&self, ns: &crate::MountNs) {
        let root = self.root().and_then(|p| ns.translate(&p));
        let cwd = self.cwd().and_then(|p| ns.translate(&p));
        if let Some(r) = root {
            self.set_root(r);
        }
        if let Some(c) = cwd {
            self.set_cwd(c);
        }
        self.view.store(ns.id, Ordering::Release);
    }
    /// SX-3a: setns(2) into a view and pivot_root(2)'s `chroot_fs_refs`: root and cwd set at once.
    pub fn place(&self, ns: &crate::MountNs, root: Path, cwd: Path) {
        self.set_root(root);
        self.set_cwd(cwd);
        self.view.store(ns.id, Ordering::Release);
    }
    /// `chroot_fs_refs` for this state: root and cwd that are `old` become `new`, compared and replaced under
    /// both locks at once (Linux `fs->lock`), so a chroot or chdir racing with it is neither lost nor half
    /// applied. Returns how many of the two were replaced; the old places are dropped after the locks.
    pub fn replace_refs(&self, old: &Path, new: &Path) -> usize {
        let mut gone: Vec<Path> = Vec::new();
        {
            let mut root = self.root.lock();
            let mut cwd = self.cwd.lock();
            for slot in [&mut *root, &mut *cwd] {
                if slot.as_ref().is_some_and(|p| p.same(old)) {
                    gone.extend(slot.replace(new.clone()));
                }
            }
        }
        gone.len()
    }
    /// The working directory (None = the root).
    pub fn cwd(&self) -> Option<Path> {
        self.cwd.lock().clone()
    }
    pub fn set_cwd(&self, p: Path) {
        let old = self.cwd.lock().replace(p);
        drop(old);
    }
    /// The process's root (None = its mount view's root).
    pub fn root(&self) -> Option<Path> {
        self.root.lock().clone()
    }
    /// `set_fs_root`; the old root is dropped outside the lock.
    pub fn set_root(&self, p: Path) {
        let old = self.root.lock().replace(p);
        drop(old);
    }
    pub fn umask(&self) -> u32 {
        self.umask.load(Ordering::Relaxed)
    }
    /// Sets the mask; returns the old one (umask(2)).
    pub fn set_umask(&self, m: u32) -> u32 {
        self.umask.swap(m & 0o777, Ordering::Relaxed)
    }
}

impl Default for FsState {
    fn default() -> Self {
        Self::new()
    }
}

/// A taken fd number: `file` is None while the number is reserved but not yet published (Linux: the bit in
/// `open_fds` is set, `fdt->fd[fd]` still NULL), so a call can copy its results out before other threads of the
/// program can see or dup the fd (`get_unused_fd_flags` .. `fd_install`).
#[derive(Clone)]
struct Slot {
    file: Option<Arc<OpenFile>>,
    cloexec: bool,
}

/// PF-3b: what a look-up without the table lock finds at a number: the file, without keeping it open (the
/// strong reference is the table's `Slot`, dropped at close at once; only this memory waits for a grace period,
/// as Linux's RCU-freed `struct file`).
struct Pub {
    file: Weak<OpenFile>,
}

/// A view entry taken out under the table lock, dropped after it (its drop hands it to the grace queue).
type Late = Option<Retire<Pub>>;

/// Several `Late` entries (close-on-exec, exit). Without room for one more, that one drops in place, as before
/// PF-6's fixer (a bounded wait only when the grace queue has no room either).
struct LateMany(Vec<Retire<Pub>>);

impl LateMany {
    fn new() -> LateMany {
        LateMany(Vec::new())
    }
    fn keep(&mut self, r: Late) {
        if let Some(r) = r {
            if self.0.try_reserve(1).is_ok() {
                self.0.push(r);
            } else {
                drop(r);
            }
        }
    }
}

klog::counter!(VIEW_NOROOM, "vfs.fd.view_noroom");
/// The first fallback is reported (B6), later ones only counted.
static VIEW_NOROOM_SEEN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Look-ups of one table that took its lock (tests and ktests; the lockless ones count nothing: no line other
/// CPUs write).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LookupStats {
    pub locked: u64,
}

pub struct FdTable {
    fds: SpinLock<Vec<Option<Slot>>>,
    /// Lowest number that might be free (Linux `next_fd`).
    hint: AtomicUsize,
    /// The highest number ever taken (FS-4: Linux's `max_fds` follows it, select caps its `n` there).
    high: AtomicUsize,
    limit: AtomicUsize,
    /// PF-3b: the published files by number, read with no lock inside a grace section (`get`; Linux reads
    /// `fdt->fd[]` under RCU, linux-6.6/fs/file.c:888-900). Changed only under `fds`, right after the slot, so
    /// it never holds a file the slot does not; a number it lacks (no room for a node, a reserved number) is
    /// looked up under the lock.
    view: RcuIndex<Pub>,
    locked: AtomicU64,
    /// Tests: the view stays empty, every look-up takes the lock.
    #[cfg(test)]
    unviewed: bool,
}

pub const DEFAULT_NOFILE: usize = abi::NOFILE_DEFAULT;

impl FdTable {
    pub fn new() -> Self {
        FdTable {
            fds: SpinLock::named("vfs.fds", Vec::new()),
            hint: AtomicUsize::new(0),
            high: AtomicUsize::new(0),
            limit: AtomicUsize::new(DEFAULT_NOFILE),
            view: RcuIndex::named("vfs.fds.view"),
            locked: AtomicU64::new(0),
            #[cfg(test)]
            unviewed: false,
        }
    }
    /// A table whose lockless view stays empty (as after its nodes found no room): every look-up locks.
    #[cfg(test)]
    pub(crate) fn new_unviewed() -> Self {
        let mut t = FdTable::new();
        t.unviewed = true;
        t
    }
    /// Under `fds`: `fd` now holds `f` (published) for lockless look-ups. Without room for the view's nodes the
    /// number leaves the view instead (the locked look-up finds it), never keeping an older file there. Returns
    /// the entry it replaced: the caller drops it after the table lock (`Late`), because handing it to the
    /// grace queue may wait a bounded grace period when the queue has no room (PF-6 fixer).
    #[must_use]
    fn show(&self, fd: usize, f: &Arc<OpenFile>) -> Late {
        let mut w = self.view.write();
        #[cfg(test)]
        if self.unviewed {
            return w.remove(fd as u64);
        }
        match w.insert(fd as u64, Arc::new(Pub { file: Arc::downgrade(f) })) {
            Ok(old) => old,
            Err(_) => {
                VIEW_NOROOM.inc();
                if !VIEW_NOROOM_SEEN.swap(true, Ordering::Relaxed) {
                    klog::event!(Warn, "vfs", "fd.view_noroom", fd = fd as u64; "no room for the lockless fd view: this number is looked up under the table lock");
                }
                w.remove(fd as u64)
            }
        }
    }
    /// Under `fds`: `fd` no longer holds a published file. Returns the entry for the caller to drop after the
    /// table lock, as `show`.
    #[must_use]
    fn hide(&self, fd: usize) -> Late {
        self.view.write().remove(fd as u64)
    }
    /// Lowest free fd >= `min` (Linux semantics): EMFILE at RLIMIT_NOFILE, ENOMEM if the table cannot grow.
    pub fn install_flags(&self, f: Arc<OpenFile>, min: usize, cloexec: bool) -> Result<usize, Errno> {
        self.take_lowest(Some(f), min, cloexec)
    }
    /// FS-10: the open fds in number order with their files (`/proc/<pid>/fd`); reserved numbers are left out.
    /// The list is allocated outside the table's spin lock; when fds were opened between counting and filling,
    /// it is made again (a few times, then the snapshot that fit is the answer).
    pub fn open_fds(&self) -> Vec<(usize, Arc<OpenFile>)> {
        let mut v: Vec<(usize, Arc<OpenFile>)> = Vec::new();
        for _ in 0..4 {
            let want = self.fds.lock().iter().filter(|s| s.is_some()).count();
            v.clear();
            if v.try_reserve(want).is_err() {
                return v;
            }
            let mut full = false;
            {
                let fds = self.fds.lock();
                for (i, s) in fds.iter().enumerate() {
                    if let Some(f) = s.as_ref().and_then(|s| s.file.clone()) {
                        if v.len() == v.capacity() {
                            full = true;
                            break;
                        }
                        v.push((i, f));
                    }
                }
            }
            if !full {
                break;
            }
        }
        v
    }
    /// Reserves the lowest free fd >= `min` without a file (`get_unused_fd_flags`): taken for every later
    /// allocation, but `get`/`close`/fcntl answer EBADF and dup2 onto it EBUSY until `commit` publishes it.
    /// Exactly one of `commit` or `unreserve` must follow.
    pub fn reserve(&self, min: usize, cloexec: bool) -> Result<usize, Errno> {
        self.take_lowest(None, min, cloexec)
    }
    /// Publishes a reserved fd (`fd_install`). A number that is not reserved (closed meanwhile by an exit) is
    /// left alone and `f` is released.
    pub fn commit(&self, fd: usize, f: Arc<OpenFile>) {
        let mut late = None;
        let unused = {
            let mut fds = self.fds.lock();
            match fds.get_mut(fd) {
                Some(Some(slot)) if slot.file.is_none() => {
                    late = self.show(fd, &f);
                    slot.file = Some(f);
                    None
                }
                _ => Some(f),
            }
        };
        if unused.is_some() {
            klog::event!(Warn, "vfs", "fd.commit_unreserved", fd = fd; "an fd to publish was not reserved (closed meanwhile): its file is released");
        }
        drop(unused);
        drop(late);
    }
    /// Gives a reserved fd back (`put_unused_fd`); a published fd is left alone.
    pub fn unreserve(&self, fd: usize) {
        let mut fds = self.fds.lock();
        if let Some(slot) = fds.get_mut(fd)
            && slot.as_ref().is_some_and(|s| s.file.is_none())
        {
            *slot = None;
            if fd < self.hint.load(Ordering::Relaxed) {
                self.hint.store(fd, Ordering::Relaxed);
            }
        }
    }
    fn take_lowest(&self, f: Option<Arc<OpenFile>>, min: usize, cloexec: bool) -> Result<usize, Errno> {
        let limit = self.limit();
        loop {
            let want = {
                let mut fds = self.fds.lock();
                let mut i = min.max(if min == 0 { self.hint.load(Ordering::Relaxed) } else { min });
                while i < fds.len() && fds[i].is_some() {
                    i += 1;
                }
                if i >= limit {
                    return Err(EMFILE);
                }
                if i < fds.len() {
                    let late = f.as_ref().and_then(|f| self.show(i, f));
                    fds[i] = Some(Slot { file: f, cloexec });
                    self.high.fetch_max(i, Ordering::Relaxed);
                    if min == 0 {
                        self.hint.store(i + 1, Ordering::Relaxed);
                    }
                    drop(fds);
                    drop(late);
                    return Ok(i);
                }
                (i + 1).max(fds.len() * 2).min(limit)
            };
            self.grow_to(want)?;
        }
    }
    /// Makes the table at least `want` long. The new table is allocated and filled outside the spin lock (as
    /// Linux's `expand_fdtable`); only moving the existing slots happens under it. Tables never shrink.
    fn grow_to(&self, want: usize) -> Result<(), Errno> {
        loop {
            let len = self.fds.lock().len();
            if len >= want {
                return Ok(());
            }
            #[cfg(test)]
            if crate::pf3_tests::grow_fails() {
                return Err(ENOMEM);
            }
            let mut v: Vec<Option<Slot>> = Vec::new();
            v.try_reserve_exact(want).map_err(|_| ENOMEM)?;
            v.resize(want, None);
            let old = {
                let mut fds = self.fds.lock();
                if fds.len() >= want {
                    return Ok(());
                }
                if fds.len() != len {
                    // Grown meanwhile to something still too short: start again with the new length.
                    continue;
                }
                for (dst, src) in v.iter_mut().zip(fds.iter_mut()) {
                    *dst = src.take();
                }
                core::mem::replace(&mut *fds, v)
            };
            // The old (now empty) table is freed outside the lock.
            drop(old);
            return Ok(());
        }
    }
    /// Whether an fd number below RLIMIT_NOFILE is free (Linux allocates the number before the walk, so a full
    /// table answers EMFILE before anything else). Starts at the `next_fd` hint: amortised O(1).
    pub fn has_free(&self) -> bool {
        let limit = self.limit();
        let fds = self.fds.lock();
        let mut i = self.hint.load(Ordering::Relaxed);
        while i < fds.len() && i < limit && fds[i].is_some() {
            i += 1;
        }
        if i < fds.len() && i < limit {
            // Everything below `i` is taken: a later install may start there.
            self.hint.store(i, Ordering::Relaxed);
        }
        i < limit
    }
    pub fn install_min(&self, f: Arc<OpenFile>, min: usize) -> Result<usize, Errno> {
        self.install_flags(f, min, false)
    }
    pub fn install(&self, f: Arc<OpenFile>) -> Result<usize, Errno> {
        self.install_flags(f, 0, false)
    }
    /// Puts `f` at number `fd` (dup2/dup3, stdio set-up), closing what was there. EBADF at or above the limit.
    pub fn install_at_flags(&self, fd: usize, f: Arc<OpenFile>, cloexec: bool) -> Result<(), Errno> {
        if fd >= self.limit() {
            return Err(EBADF);
        }
        self.grow_to(fd + 1)?;
        let late;
        let old = {
            let mut fds = self.fds.lock();
            // Tables never shrink, so the slot exists after `grow_to`.
            match fds.get_mut(fd) {
                // `do_dup2`: a number reserved by a call still in progress is EBUSY.
                Some(Some(s)) if s.file.is_none() => return Err(EBUSY),
                Some(slot) => {
                    self.high.fetch_max(fd, Ordering::Relaxed);
                    let old = slot.replace(Slot { file: Some(f.clone()), cloexec });
                    late = self.show(fd, &f);
                    old
                }
                None => return Err(ENOMEM),
            }
        };
        // The replaced file is released outside the table lock (its last close may wake or take locks).
        drop(old);
        drop(late);
        Ok(())
    }
    pub fn install_at(&self, fd: usize, f: Arc<OpenFile>) -> Result<(), Errno> {
        self.install_at_flags(fd, f, false)
    }
    /// The open file at `fd` (`fget_raw`), with a reference of its own. PF-3b: first without the table lock
    /// (Linux `__fget_files_rcu`, linux-6.6/fs/file.c:888-900): inside a grace section the view's entry, its file
    /// taken with `Weak::upgrade` (inc-not-zero: a file whose last reference went never comes back). The load of
    /// the entry is the moment the look-up answers for: the entry names one file (its memory is not reused while
    /// the section lives), and every change of the number replaces or removes the entry under the table lock,
    /// after the slot, so no other look-up sees the number in between. Linux reads the slot a second time because
    /// its `struct file` memory may be another file by then (SLAB_TYPESAFE_BY_RCU); a `Weak` cannot. Else (a
    /// number the view lacks, or the file just went) the locked look-up.
    pub fn get(&self, fd: usize) -> Result<Arc<OpenFile>, Errno> {
        if let Some(f) = self.get_lockless(fd) {
            return Ok(f);
        }
        self.locked.fetch_add(1, Ordering::Relaxed);
        let fds = self.fds.lock();
        fds.get(fd).and_then(|s| s.as_ref()).and_then(|s| s.file.clone()).ok_or(EBADF)
    }
    fn get_lockless(&self, fd: usize) -> Option<Arc<OpenFile>> {
        let s = grace::enter()?;
        self.view.get(&s, fd as u64)?.file.upgrade()
    }
    /// Look-ups of this table that took its lock (PF-3 tests and ktests).
    pub fn lookup_stats(&self) -> LookupStats {
        LookupStats { locked: self.locked.load(Ordering::Relaxed) }
    }
    pub fn cloexec(&self, fd: usize) -> Result<bool, Errno> {
        self.fds.lock().get(fd).and_then(|s| s.as_ref()).filter(|s| s.file.is_some()).map(|s| s.cloexec).ok_or(EBADF)
    }
    pub fn set_cloexec(&self, fd: usize, on: bool) -> Result<(), Errno> {
        match self.fds.lock().get_mut(fd).and_then(|s| s.as_mut()).filter(|s| s.file.is_some()) {
            Some(s) => {
                s.cloexec = on;
                Ok(())
            }
            None => Err(EBADF),
        }
    }
    pub fn close(&self, fd: usize) -> Result<(), Errno> {
        self.close_get(fd).map(drop)
    }
    /// close(2)'s table half (Linux `close_fd_get_file` / `pick_file`): takes the file out of `fd` and hands it
    /// to the caller, who does `filp_close`'s work (the POSIX locks) on the very file that left the table and
    /// then drops it, so a racing dup2 or open on the same number cannot make it act on another file. EBADF for
    /// a free number and for one a call in progress reserved.
    pub fn close_get(&self, fd: usize) -> Result<Arc<OpenFile>, Errno> {
        // Declared before the guard, so it drops after the lock.
        let late;
        // The hint changes only under the table lock ("every number below it is taken").
        let mut fds = self.fds.lock();
        let Some(slot) = fds.get_mut(fd).filter(|s| s.as_ref().is_some_and(|s| s.file.is_some())) else {
            return Err(EBADF);
        };
        let file = slot.take().and_then(|s| s.file).ok_or(EBADF)?;
        late = self.hide(fd);
        if fd < self.hint.load(Ordering::Relaxed) {
            self.hint.store(fd, Ordering::Relaxed);
        }
        drop(fds);
        drop(late);
        // Returned (and so dropped) outside the lock: the last close may wake or take locks.
        Ok(file)
    }
    /// dup2/dup3's table half (Linux `ksys_dup3` + `do_dup2`, linux-6.6/fs/file.c:1266, :1105): `old`'s file
    /// (an O_PATH one too) goes to `new` with its own close-on-exec flag. The look-up of `old` and the
    /// replacement are one step under the table lock, so a close of `old` racing with it comes either before
    /// (EBADF, nothing changed) or after (the copy stays). Returns what `new` held before, for the caller's
    /// `filp_close` (its POSIX locks go) and drop outside the lock. EBADF at or above the limit and for a free
    /// or reserved `old`; EBUSY when a call in progress reserved `new`; ENOMEM when the table cannot grow to
    /// `new` (only after `old` was found, the order of `ksys_dup3`). The table grows to `new` before `old` is
    /// looked up, as `ksys_dup3` runs `expand_files` before `files_lookup_fd_locked` (file.c:1247-1248): a
    /// dup2 of a closed fd to a high number below the limit leaves the table as long as Linux leaves it.
    pub fn dup_to(&self, old: usize, new: usize, cloexec: bool) -> Result<Option<Arc<OpenFile>>, Errno> {
        if new >= self.limit() {
            return Err(EBADF);
        }
        let grown = self.grow_to(new + 1);
        // Declared before the guard, so it drops after the lock.
        let late;
        let mut fds = self.fds.lock();
        let f = fds.get(old).and_then(|s| s.as_ref()).and_then(|s| s.file.clone()).ok_or(EBADF)?;
        grown?;
        // Tables never shrink, so after a successful `grow_to` the slot exists.
        let slot = fds.get_mut(new).ok_or(ENOMEM)?;
        if slot.as_ref().is_some_and(|s| s.file.is_none()) {
            return Err(EBUSY);
        }
        self.high.fetch_max(new, Ordering::Relaxed);
        let replaced = slot.replace(Slot { file: Some(f.clone()), cloexec }).and_then(|s| s.file);
        late = self.show(new, &f);
        drop(fds);
        drop(late);
        Ok(replaced)
    }
    /// execve: closes every close-on-exec fd (AB-4 calls it) and returns the files that left, in one pass under
    /// the table lock (Linux `do_close_on_exec` calls `filp_close` on each: the caller drops their POSIX locks
    /// and then the files, outside the lock).
    pub fn close_on_exec(&self) -> Vec<Arc<OpenFile>> {
        let mut late = LateMany::new();
        let mut fds = self.fds.lock();
        self.hint.store(0, Ordering::Relaxed);
        let mut out = Vec::new();
        for (i, s) in fds.iter_mut().enumerate() {
            if s.as_ref().is_some_and(|s| s.cloexec && s.file.is_some())
                && let Some(f) = s.take().and_then(|s| s.file)
            {
                late.keep(self.hide(i));
                out.push(f);
            }
        }
        drop(fds);
        drop(late);
        out
    }
    /// exit (Linux `exit_files`): closes every fd at once, so pipe readers see EOF and writers EPIPE when a
    /// program ends, not only when its process object is freed (FS-3 change request to AB: call it when the
    /// process ends). The table stays usable (empty).
    pub fn close_all(&self) {
        let mut late = LateMany::new();
        let gone: Vec<Slot> = {
            let mut fds = self.fds.lock();
            self.hint.store(0, Ordering::Relaxed);
            self.view.write().drain_from(0, &mut |_, r| late.keep(Some(r)));
            fds.iter_mut().filter_map(|s| s.take()).collect()
        };
        drop(gone);
        drop(late);
    }
    /// fork (`dup_fd`) and execve's `unshare_files`: a copy sharing the open files (positions are shared, as
    /// Linux) with the same limit. The file-system state is not part of the table (`FsState`).
    pub fn duplicate(&self) -> Result<FdTable, Errno> {
        let t = FdTable::new();
        // Allocated outside the source's spin lock; under it only the slots are cloned (Linux `dup_fd`).
        let copy = loop {
            let len = self.fds.lock().len();
            let mut v: Vec<Option<Slot>> = Vec::new();
            v.try_reserve_exact(len).map_err(|_| ENOMEM)?;
            let src = self.fds.lock();
            if src.len() > len {
                continue;
            }
            // A number another thread reserved but has not published yet is free in the child (`dup_fd`).
            v.extend(src.iter().map(|s| s.clone().filter(|s| s.file.is_some())));
            break v;
        };
        // The child's table is sized by its highest open number (`sane_fdtable_size`).
        t.high.store(copy.iter().rposition(|s| s.is_some()).unwrap_or(0), Ordering::Relaxed);
        let mut fds = t.fds.lock();
        for (i, s) in copy.iter().enumerate() {
            if let Some(f) = s.as_ref().and_then(|s| s.file.as_ref()) {
                // A fresh table: nothing to replace, so nothing waits under the lock.
                drop(t.show(i, f));
            }
        }
        *fds = copy;
        drop(fds);
        t.set_limit(self.limit());
        Ok(t)
    }
    /// RLIMIT_NOFILE (soft). Values above `NR_OPEN` are clamped (prlimit refuses them earlier with EPERM).
    pub fn set_limit(&self, n: usize) {
        self.limit.store(n.min(abi::NR_OPEN), Ordering::Relaxed);
    }
    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::Relaxed)
    }
    pub fn count(&self) -> usize {
        self.fds.lock().iter().filter(|f| f.as_ref().is_some_and(|s| s.file.is_some())).count()
    }
    /// FS-4: whether `fd` is taken (Linux's `open_fds` bit: also an O_PATH fd and a number reserved by a call
    /// in progress), as select's `max_select_fd` asks.
    pub fn is_open(&self, fd: usize) -> bool {
        self.fds.lock().get(fd).is_some_and(|s| s.is_some())
    }
    /// FS-4: Linux's `fdt->max_fds` for this table: 64 (NR_OPEN_DEFAULT) until a number at or above it was
    /// taken, then `expand_fdtable`'s size for the highest number (multiples of 128, powers of two).
    pub fn max_fds(&self) -> usize {
        let nr = self.high.load(Ordering::Relaxed);
        if nr < 64 { 64 } else { (nr / 128 + 1).next_power_of_two() * 128 }
    }
    /// Slots the table has now (`FDSize` of /proc/<pid>/status; Linux starts at 64, NR_OPEN_DEFAULT).
    pub fn capacity(&self) -> usize {
        self.fds.lock().len().max(64)
    }
}

impl Default for FdTable {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FdTable {
    fn drop(&mut self) {
        // Files are released one by one outside the lock.
        let all = core::mem::take(self.fds.get_mut());
        drop(all);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::MountNs;
    #[test]
    fn lowest_free_fd_and_limit() {
        let ns = MountNs::new_empty();
        ns.create_file("/f", 0o644, alloc::vec![9; 10]).unwrap();
        let t = FdTable::new();
        let of = || OpenFile::new(ns.open("/f").unwrap(), OpenFlags { read: true, ..Default::default() });
        assert_eq!(t.install(of()).unwrap(), 0);
        assert_eq!(t.install(of()).unwrap(), 1);
        t.close(0).unwrap();
        assert_eq!(t.install(of()).unwrap(), 0);
        t.set_limit(3);
        assert_eq!(t.install(of()).unwrap(), 2);
        assert_eq!(t.install(of()).err(), Some(EMFILE));
        assert_eq!(t.install_min(of(), 1).err(), Some(EMFILE));
        let f = t.get(1).unwrap();
        let mut b = [0u8; 4];
        assert_eq!(f.read(&mut b).unwrap(), 4);
        assert_eq!(f.seek(-2, 2).unwrap(), 8);
        assert_eq!(f.read(&mut b).unwrap(), 2);
        assert_eq!(f.write(&b).err(), Some(EBADF));
        assert_eq!(t.install_at(3, of()).err(), Some(EBADF));
        t.set_cloexec(1, true).unwrap();
        t.close_on_exec();
        assert_eq!(t.get(1).err(), Some(EBADF));
        assert_eq!(t.count(), 2);
    }

    #[test]
    fn reserved_numbers_are_taken_but_invisible_until_published() {
        let ns = MountNs::new_empty();
        ns.create_file("/f", 0o644, alloc::vec![1; 4]).unwrap();
        let t = FdTable::new();
        let of = || OpenFile::new(ns.open("/f").unwrap(), OpenFlags { read: true, ..Default::default() });
        assert_eq!(t.install(of()).unwrap(), 0);
        let r = t.reserve(0, true).unwrap();
        assert_eq!(r, 1);
        // Taken for allocation, invisible otherwise (EBADF), dup2 onto it EBUSY.
        assert_eq!(t.install(of()).unwrap(), 2);
        assert_eq!(t.get(r).err(), Some(EBADF));
        assert_eq!(t.cloexec(r).err(), Some(EBADF));
        assert_eq!(t.set_cloexec(r, false).err(), Some(EBADF));
        assert_eq!(t.close(r).err(), Some(EBADF));
        assert_eq!(t.install_at(r, of()).err(), Some(EBUSY));
        assert_eq!(t.count(), 2);
        // A fork meanwhile does not inherit the reservation.
        let child = t.duplicate().unwrap();
        assert_eq!(child.install(of()).unwrap(), 1);
        // exec skips it; commit publishes it with the reserved close-on-exec flag.
        t.close_on_exec();
        t.commit(r, of());
        assert!(t.get(r).is_ok());
        assert_eq!(t.cloexec(r), Ok(true));
        // commit of a number that is not reserved leaves the table alone.
        let before = Arc::as_ptr(&t.get(r).unwrap());
        t.commit(r, of());
        assert_eq!(Arc::as_ptr(&t.get(r).unwrap()), before);
        // unreserve frees the number again and only a reservation.
        let q = t.reserve(0, false).unwrap();
        assert_eq!(q, 3);
        t.unreserve(q);
        t.unreserve(0);
        assert!(t.get(0).is_ok());
        assert_eq!(t.install(of()).unwrap(), 3);
    }

    /// FS-16: `copy` (copy_fs_struct) keeps cwd, root and umask; afterwards the two states are independent.
    #[test]
    fn fs_state_copy_lives_apart() {
        let ns = MountNs::new_empty();
        ns.create_file("/d", 0o644, alloc::vec![]).unwrap();
        let a = FsState::new();
        a.set_umask(0o077);
        a.set_root(ns.root_path());
        a.set_cwd(ns.root_path());
        let b = a.copy();
        assert!(b.umask() == 0o077 && b.root().is_some() && b.cwd().is_some(), "copy_fs_struct keeps cwd, root and umask");
        assert_eq!(a.set_umask(0o022), 0o077);
        assert!(b.umask() == 0o077 && FsState::new().umask() == 0o022 && FsState::new().root().is_none(), "then lives apart");
    }
}

/// The locked position; its value goes to `pos_mirror` on unlock.
struct PosGuard<'a> {
    g: ksync::MutexGuard<'a, u64>,
    mirror: &'a AtomicU64,
}

impl core::ops::Deref for PosGuard<'_> {
    type Target = u64;
    fn deref(&self) -> &u64 {
        &self.g
    }
}

impl core::ops::DerefMut for PosGuard<'_> {
    fn deref_mut(&mut self) -> &mut u64 {
        &mut self.g
    }
}

impl Drop for PosGuard<'_> {
    fn drop(&mut self) {
        self.mirror.store(*self.g, Ordering::Relaxed);
    }
}
