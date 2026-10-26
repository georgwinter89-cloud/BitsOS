//! Open files and per-process fd tables (owner FS).
//!
//! `OpenFile` is Linux's `struct file`: the object, its status flags (F_GETFL/F_SETFL), its position and, when
//! it was opened by name, the place it was found (`Path`, for fstat, getdents64, fchdir and *at calls).
//! `FdTable` maps numbers to open files with a close-on-exec bit per number; it grows on demand up to the
//! process's RLIMIT_NOFILE (default 1024, ceiling `NR_OPEN` like Linux: R1 says this is the only limit).
//! The table also carries the process's file-system state (cwd, umask; Linux's `fs_struct`) until AB-4's
//! clone splits CLONE_FS from CLONE_FILES.

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

/// The process's file-system state (Linux `fs_struct`).
pub struct FsState {
    cwd: SpinLock<Option<Path>>,
    umask: AtomicU32,
}

impl FsState {
    pub fn new() -> Self {
        FsState { cwd: SpinLock::named("vfs.fs.cwd", None), umask: AtomicU32::new(0o022) }
    }
    /// The working directory (None = the view's root).
    pub fn cwd(&self) -> Option<Path> {
        self.cwd.lock().clone()
    }
    pub fn set_cwd(&self, p: Path) {
        let old = self.cwd.lock().replace(p);
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

pub struct FdTable {
    fds: SpinLock<Vec<Option<Slot>>>,
    /// Lowest number that might be free (Linux `next_fd`).
    hint: AtomicUsize,
    /// The highest number ever taken (FS-4: Linux's `max_fds` follows it, select caps its `n` there).
    high: AtomicUsize,
    limit: AtomicUsize,
    pub fs: FsState,
}

pub const DEFAULT_NOFILE: usize = abi::NOFILE_DEFAULT;

impl FdTable {
    pub fn new() -> Self {
        FdTable { fds: SpinLock::named("vfs.fds", Vec::new()), hint: AtomicUsize::new(0), high: AtomicUsize::new(0), limit: AtomicUsize::new(DEFAULT_NOFILE), fs: FsState::new() }
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
        let unused = {
            let mut fds = self.fds.lock();
            match fds.get_mut(fd) {
                Some(Some(slot)) if slot.file.is_none() => {
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
                    fds[i] = Some(Slot { file: f, cloexec });
                    self.high.fetch_max(i, Ordering::Relaxed);
                    if min == 0 {
                        self.hint.store(i + 1, Ordering::Relaxed);
                    }
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
        let old = {
            let mut fds = self.fds.lock();
            // Tables never shrink, so the slot exists after `grow_to`.
            match fds.get_mut(fd) {
                // `do_dup2`: a number reserved by a call still in progress is EBUSY.
                Some(Some(s)) if s.file.is_none() => return Err(EBUSY),
                Some(slot) => {
                    self.high.fetch_max(fd, Ordering::Relaxed);
                    slot.replace(Slot { file: Some(f), cloexec })
                }
                None => return Err(ENOMEM),
            }
        };
        // The replaced file is released outside the table lock (its last close may wake or take locks).
        drop(old);
        Ok(())
    }
    pub fn install_at(&self, fd: usize, f: Arc<OpenFile>) -> Result<(), Errno> {
        self.install_at_flags(fd, f, false)
    }
    pub fn get(&self, fd: usize) -> Result<Arc<OpenFile>, Errno> {
        self.fds.lock().get(fd).and_then(|s| s.as_ref()).and_then(|s| s.file.clone()).ok_or(EBADF)
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
        // The hint changes only under the table lock ("every number below it is taken").
        let old = {
            let mut fds = self.fds.lock();
            let old = match fds.get_mut(fd) {
                Some(slot @ Some(_)) if slot.as_ref().is_some_and(|s| s.file.is_some()) => slot.take(),
                _ => return Err(EBADF),
            };
            if fd < self.hint.load(Ordering::Relaxed) {
                self.hint.store(fd, Ordering::Relaxed);
            }
            old
        };
        drop(old);
        Ok(())
    }
    /// execve: closes every close-on-exec fd (AB-4 calls it).
    pub fn close_on_exec(&self) {
        let gone: Vec<Slot> = {
            let mut fds = self.fds.lock();
            self.hint.store(0, Ordering::Relaxed);
            fds.iter_mut().filter(|s| s.as_ref().is_some_and(|s| s.cloexec && s.file.is_some())).filter_map(|s| s.take()).collect()
        };
        drop(gone);
    }
    /// exit (Linux `exit_files`): closes every fd at once, so pipe readers see EOF and writers EPIPE when a
    /// program ends, not only when its process object is freed (FS-3 change request to AB: call it when the
    /// process ends). The table stays usable (empty).
    pub fn close_all(&self) {
        let gone: Vec<Slot> = {
            let mut fds = self.fds.lock();
            self.hint.store(0, Ordering::Relaxed);
            fds.iter_mut().filter_map(|s| s.take()).collect()
        };
        drop(gone);
    }
    /// fork: a copy sharing the open files (positions are shared, as Linux), same limit, cwd and umask.
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
        *t.fds.lock() = copy;
        t.set_limit(self.limit());
        if let Some(c) = self.fs.cwd() {
            t.fs.set_cwd(c);
        }
        t.fs.set_umask(self.fs.umask());
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
