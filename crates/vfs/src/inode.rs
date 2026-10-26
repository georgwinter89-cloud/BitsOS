//! Inodes: metadata every file system keeps (mode, owner, link count, times) and the content of tmpfs nodes
//! (regular files are their page cache since FS-5, symlink targets, device and proc nodes). A later disk file
//! system plugs in through `Data::External`.

use crate::abi::{self, Attr, Time};
use crate::node::File;
use crate::pagecache::PageCache;
use crate::Errno;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use ksync::SpinLock;
use uapi::errno::*;

pub const PAGE: u64 = 4096;

/// Who asks (DAC). uid 0 passes every read/write check and every exec check where any x bit is set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cred {
    pub uid: u32,
    pub gid: u32,
}

static GROUP_CHECK: ksync::Once<fn(Cred, u32) -> bool> = ksync::Once::new();

/// AB-13: the process layer's supplementary-group check (vfs-sys installs `proc::cred::current_in_group`),
/// kept as a hook so `Cred` stays `Copy`.
pub fn set_group_check(f: fn(Cred, u32) -> bool) {
    GROUP_CHECK.get_or_init(|| f);
}

static CURRENT_CRED: ksync::Once<fn() -> Option<Cred>> = ksync::Once::new();

/// FS-8a: the calling process's file-system credentials for code that has no `Cred` passed in (a disk file
/// system's block reservation at `write`: `current_fsuid()`); vfs-sys installs the process layer's.
pub fn set_current_cred(f: fn() -> Option<Cred>) {
    CURRENT_CRED.get_or_init(|| f);
}

/// The caller's (fsuid, fsgid); None in a kernel thread or before the hook is installed.
pub fn current_cred() -> Option<Cred> {
    CURRENT_CRED.get().and_then(|f| f())
}

/// Linux `in_group_p` (linux-6.6/kernel/groups.c): the fs gid or one of the supplementary groups.
pub fn in_group(cred: Cred, gid: u32) -> bool {
    cred.gid == gid || GROUP_CHECK.get().is_some_and(|f| f(cred, gid))
}

impl Cred {
    pub const ROOT: Cred = Cred { uid: 0, gid: 0 };
}

/// Access bits for `permission`.
pub const MAY_EXEC: u32 = 1;
pub const MAY_WRITE: u32 = 2;
pub const MAY_READ: u32 = 4;

#[derive(Clone, Copy, Debug, Default)]
struct Times {
    atime: Time,
    mtime: Time,
    ctime: Time,
    btime: Time,
}

/// The time stamp for new events: monotonic since boot (V2 has no wall clock yet; clock_gettime answers the
/// same clock, so programs see consistent values).
/// The clock file times come from (INT-A1: the kernel installs the wall clock, so file times agree with
/// CLOCK_REALTIME); without one, the monotonic clock since boot.
static CLOCK: ksync::Once<fn() -> u64> = ksync::Once::new();

/// Installs the file-time clock (ns since the Unix epoch). The first call wins.
pub fn set_clock(f: fn() -> u64) {
    CLOCK.get_or_init(|| f);
}

pub fn now() -> Time {
    Time::from_ns(CLOCK.get().map(|f| f()).or_else(ksync::now_ns).unwrap_or(0))
}

pub struct Meta {
    pub ino: u64,
    mode: AtomicU32,
    uid: AtomicU32,
    gid: AtomicU32,
    nlink: AtomicU32,
    times: SpinLock<Times>,
}

impl Meta {
    pub fn new(ino: u64, mode: u32, cred: Cred) -> Meta {
        let t = now();
        let nlink = if mode & uapi::S_IFMT == uapi::S_IFDIR { 2 } else { 1 };
        Meta {
            ino,
            mode: AtomicU32::new(mode),
            uid: AtomicU32::new(cred.uid),
            gid: AtomicU32::new(cred.gid),
            nlink: AtomicU32::new(nlink),
            times: SpinLock::named("vfs.inode.times", Times { atime: t, mtime: t, ctime: t, btime: t }),
        }
    }
    pub fn mode(&self) -> u32 {
        self.mode.load(Ordering::Relaxed)
    }
    pub fn set_perm(&self, perm: u32) {
        let m = self.mode();
        self.mode.store((m & uapi::S_IFMT) | (perm & 0o7777), Ordering::Relaxed);
        self.touch_ctime();
    }
    pub fn set_owner(&self, uid: u32, gid: u32) {
        self.uid.store(uid, Ordering::Relaxed);
        self.gid.store(gid, Ordering::Relaxed);
    }
    pub fn uid(&self) -> u32 {
        self.uid.load(Ordering::Relaxed)
    }
    pub fn gid(&self) -> u32 {
        self.gid.load(Ordering::Relaxed)
    }
    pub fn nlink(&self) -> u32 {
        self.nlink.load(Ordering::Relaxed)
    }
    /// Returns the count before.
    pub fn inc_nlink(&self) -> u32 {
        self.nlink.fetch_add(1, Ordering::Relaxed)
    }
    /// Returns the count before (0 stays 0).
    pub fn dec_nlink(&self) -> u32 {
        let mut n = self.nlink.load(Ordering::Relaxed);
        while let Err(now) = self.nlink.compare_exchange_weak(n, n.saturating_sub(1), Ordering::Relaxed, Ordering::Relaxed) {
            n = now;
        }
        n
    }
    /// A disk file system's node as found on its disk (FS-8).
    pub fn set_nlink(&self, n: u32) {
        self.nlink.store(n, Ordering::Relaxed);
    }
    pub fn clear_nlink(&self) {
        self.nlink.store(0, Ordering::Relaxed);
    }
    pub fn is_dir(&self) -> bool {
        self.mode() & uapi::S_IFMT == uapi::S_IFDIR
    }
    pub fn file_type(&self) -> u32 {
        self.mode() & uapi::S_IFMT
    }
    /// Content changed: mtime and ctime.
    pub fn touch_mtime(&self) {
        let t = now();
        let mut s = self.times.lock();
        s.mtime = t;
        s.ctime = t;
    }
    pub fn touch_ctime(&self) {
        self.times.lock().ctime = now();
    }
    pub fn set_mtime(&self, t: Time) {
        let mut s = self.times.lock();
        s.mtime = t;
        s.atime = t;
    }
    /// All four times (a disk file system's node as found on its disk).
    pub fn set_times(&self, atime: Time, mtime: Time, ctime: Time, btime: Time) {
        *self.times.lock() = Times { atime, mtime, ctime, btime };
    }
    /// (atime, mtime, ctime).
    pub fn times(&self) -> (Time, Time, Time) {
        let t = *self.times.lock();
        (t.atime, t.mtime, t.ctime)
    }
    /// FS-1b `setattr_copy` (linux-6.6/fs/attr.c:303): owner, times and mode of `a`, under the times lock so a
    /// concurrent stat sees all or nothing of one change. `a.mode` keeps the type (the caller built it so).
    pub fn apply(&self, a: &crate::setattr::SetAttr) {
        use crate::setattr::*;
        let mut t = self.times.lock();
        if a.valid & ATTR_UID != 0 {
            self.uid.store(a.uid, Ordering::Relaxed);
        }
        if a.valid & ATTR_GID != 0 {
            self.gid.store(a.gid, Ordering::Relaxed);
        }
        if a.valid & ATTR_ATIME != 0 {
            t.atime = a.atime;
        }
        if a.valid & ATTR_MTIME != 0 {
            t.mtime = a.mtime;
        }
        if a.valid & ATTR_CTIME != 0 {
            t.ctime = a.ctime;
        }
        if a.valid & ATTR_MODE != 0 {
            self.mode.store((self.mode() & uapi::S_IFMT) | (a.mode & 0o7777), Ordering::Relaxed);
        }
    }

    /// stat of an inode that only has metadata (FS-3: pipefs and anon_inodefs inodes): size 0, no blocks.
    pub fn attr(&self, dev: u64) -> Attr {
        let t = *self.times.lock();
        Attr {
            dev,
            ino: self.ino,
            mode: self.mode(),
            nlink: self.nlink(),
            uid: self.uid(),
            gid: self.gid(),
            rdev: 0,
            size: 0,
            blksize: PAGE as u32,
            blocks: 0,
            atime: t.atime,
            mtime: t.mtime,
            ctime: t.ctime,
            btime: t.btime,
            mnt_id: 0,
            mount_root: false,
        }
    }

    /// `generic_permission` without ACLs and capabilities other than uid 0's override.
    pub fn permission(&self, cred: Cred, want: u32) -> Result<(), Errno> {
        let mode = self.mode();
        if cred.uid == 0 {
            // CAP_DAC_OVERRIDE: everything except exec of a non-directory without any x bit.
            if want & MAY_EXEC != 0 && !self.is_dir() && mode & 0o111 == 0 {
                return Err(EACCES);
            }
            return Ok(());
        }
        let bits = if cred.uid == self.uid() {
            (mode >> 6) & 7
        } else if in_group(cred, self.gid()) {
            (mode >> 3) & 7
        } else {
            mode & 7
        };
        if want & !bits & 7 != 0 { Err(EACCES) } else { Ok(()) }
    }
}

/// FS-1's names for the tmpfs budget, kept for their callers: since FS-5 the system pool of `pool` (R2).
pub fn set_tmpfs_page_limit(pages: Option<u64>) {
    crate::pool::set_system_limit(pages);
}

/// The system pool's limit in pages (u64::MAX when there is no frame allocator to measure).
pub fn tmpfs_page_limit() -> u64 {
    crate::pool::system_limit()
}

/// Frames tmpfs files hold now (all instances; `/proc/meminfo` Shmem).
pub fn tmpfs_pages_used() -> u64 {
    crate::pagecache::tmpfs_frames()
}

/// A file system node's content and type-specific operations beyond tmpfs (FS-7/FS-8 put their inodes here).
pub trait InodeOps: Send + Sync {
    /// The file's page cache, when its content goes through one (FS-6..8; then it can be mapped).
    fn page_cache(&self) -> Option<&PageCache> {
        None
    }
    fn size(&self) -> u64;
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno>;
    fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno>;
    fn truncate(&self, size: u64) -> Result<(), Errno>;
    /// fsync/fdatasync: dirty pages and the file system's metadata to the disk (FAT: the directory entry,
    /// FSInfo, then the device's cache flush when `why.durable()`).
    fn sync(&self, why: crate::pagecache::SyncKind) -> Result<(), Errno> {
        match self.page_cache() {
            Some(c) => c.sync_all(why),
            None => Ok(()),
        }
    }
    /// 512-byte blocks the file holds on its disk (`st_blocks`).
    fn blocks(&self) -> u64 {
        0
    }
    /// FS-1b: fallocate after the syscall's checks; Ok(true) when the content changed (mtime and ctime follow).
    /// Default: the file system has none (EOPNOTSUPP, as Linux without `f_op->fallocate`).
    fn fallocate(&self, _mode: u32, _offset: u64, _len: u64) -> Result<bool, Errno> {
        Err(EOPNOTSUPP)
    }
}

pub enum Data {
    Dir,
    /// A tmpfs regular file: its page cache is its content.
    Reg(PageCache),
    Symlink(Vec<u8>),
    /// A device registered with `devnode!`: opening gives its `File`.
    Device(Arc<dyn File>),
    /// A `procfile!`: rendered once per open.
    Proc(fn() -> String),
    External(Box<dyn InodeOps>),
    /// A file of a synthetic file system whose content comes from its owner (FS-2, `synth`).
    Synth(Arc<dyn crate::synth::FileSource>),
    /// A directory of a synthetic file system (`/proc`, `/sys`): static names in the dentry tree plus computed
    /// ones from its sources.
    SynthDir(Arc<crate::synth::SynthDir>),
}

pub struct Inode {
    pub meta: Meta,
    pub data: Data,
    /// An O_TMPFILE inode without O_EXCL: it may get its first name by linkat although nlink is 0
    /// (Linux `I_LINKABLE`); cleared by that link.
    pub linkable: AtomicBool,
    /// Linux `S_IMMUTABLE` (proc's process directories): any write access to it is EPERM, before the mode bits.
    pub immutable: AtomicBool,
    /// Its metadata charge (tmpfs inodes: `INODE_BYTES` plus a symlink's target), given back with the inode.
    _charge: Option<crate::pool::MetaCharge>,
    /// FS-10: a proc "magic" link (`/proc/<pid>/fd/<n>`): following it leads to the open file's place itself
    /// (Linux `nd_jump_link`), not to its text (`Data::Symlink`, what readlink shows).
    pub magic: Option<Arc<dyn crate::synth::MagicLink>>,
    /// Linux `i_writecount`: opens for writing (> 0) or running programs denying them (< 0). ETXTBSY (AB-8 CR2).
    write_count: AtomicI64,
    /// FS-14: the inotify watches on it (Linux `i_fsnotify_marks`).
    pub marks: crate::inotify::Marks,
}

/// An open for writing (`get_write_access`), given back when dropped.
pub struct WriteAccess(Arc<Inode>);

impl Drop for WriteAccess {
    fn drop(&mut self) {
        self.0.write_count.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A running program's (or an interpreter being loaded) hold on its file (`deny_write_access`): opens for writing
/// and truncate answer ETXTBSY while it lives.
pub struct DenyWrite(Arc<Inode>);

impl DenyWrite {
    pub fn inode(&self) -> &Arc<Inode> {
        &self.0
    }
}

impl Drop for DenyWrite {
    fn drop(&mut self) {
        self.0.write_count.fetch_add(1, Ordering::AcqRel);
    }
}

static ALLOCATED: AtomicU64 = AtomicU64::new(0);

impl Inode {
    pub fn new(ino: u64, mode: u32, cred: Cred, data: Data) -> Arc<Inode> {
        Self::with_charge(ino, mode, cred, data, None)
    }
    /// An inode that holds `charge` (its metadata in its file system's pool) until it is freed.
    pub fn with_charge(ino: u64, mode: u32, cred: Cred, data: Data, charge: Option<crate::pool::MetaCharge>) -> Arc<Inode> {
        ALLOCATED.fetch_add(1, Ordering::Relaxed);
        Arc::new(Inode { meta: Meta::new(ino, mode, cred), data, linkable: AtomicBool::new(false), immutable: AtomicBool::new(false), _charge: charge, magic: None, write_count: AtomicI64::new(0), marks: Default::default() })
    }
    /// FS-15: one more link (`shmem_link` + `inc_nlink`): a tmpfs non-directory that had a name already takes
    /// one more inode slot (statfs `f_ffree`). The slot is decided by this call's own link-count step, and it is
    /// taken before the count grows, so a racing `drop_link` always finds it (slots >= max(nlink, 1)).
    pub fn add_link(&self) {
        let c = self._charge.as_ref().filter(|_| !self.meta.is_dir());
        if let Some(c) = c {
            c.add_slot();
        }
        if self.meta.inc_nlink() == 0
            && let Some(c) = c
        {
            c.drop_slot(); // an O_TMPFILE inode getting its first name keeps its own slot only
        }
    }
    /// FS-15: one link fewer (`shmem_unlink` + `drop_nlink`): a tmpfs non-directory that had more than one link
    /// gives one inode slot back, after the count dropped.
    pub fn drop_link(&self) {
        if self.meta.dec_nlink() > 1
            && !self.meta.is_dir()
            && let Some(c) = &self._charge
        {
            c.drop_slot();
        }
    }
    /// FS-10: a magic link showing `text` (proc's fd links; `st_size` 64 as Linux's `proc_fd_instantiate`).
    pub fn new_magic(ino: u64, mode: u32, cred: Cred, text: Vec<u8>, magic: Arc<dyn crate::synth::MagicLink>) -> Arc<Inode> {
        ALLOCATED.fetch_add(1, Ordering::Relaxed);
        Arc::new(Inode {
            meta: Meta::new(ino, mode, cred),
            data: Data::Symlink(text),
            linkable: AtomicBool::new(false),
            immutable: AtomicBool::new(false),
            _charge: None,
            magic: Some(magic),
            write_count: AtomicI64::new(0),
            marks: Default::default(),
        })
    }
    pub fn is_dir(&self) -> bool {
        self.meta.is_dir()
    }
    pub fn is_symlink(&self) -> bool {
        self.meta.file_type() == uapi::S_IFLNK
    }
    pub fn is_reg(&self) -> bool {
        self.meta.file_type() == uapi::S_IFREG
    }
    /// The page cache behind a regular file (tmpfs, or a file system that caches its files).
    pub fn page_cache(&self) -> Option<&PageCache> {
        match &self.data {
            Data::Reg(c) => Some(c),
            Data::External(o) => o.page_cache(),
            _ => None,
        }
    }
    pub fn size(&self) -> u64 {
        if self.magic.is_some() {
            return 64;
        }
        match &self.data {
            Data::Reg(c) => c.size(),
            Data::Symlink(t) => t.len() as u64,
            Data::External(o) => o.size(),
            Data::Device(f) => f.size(),
            Data::Synth(f) => f.size(),
            Data::Dir | Data::Proc(_) | Data::SynthDir(_) => 0,
        }
    }
    fn blocks(&self) -> u64 {
        match &self.data {
            Data::Reg(c) => c.pages() * (PAGE / 512),
            Data::External(o) => o.blocks(),
            _ => 0,
        }
    }
    /// `get_write_access` (linux-6.6/include/linux/fs.h `atomic_inc_unless_negative`): ETXTBSY while a running
    /// program denies writes.
    pub fn get_write_access(self: &Arc<Self>) -> Result<WriteAccess, Errno> {
        self.write_count.try_update(Ordering::AcqRel, Ordering::Acquire, |v| (v >= 0).then_some(v + 1)).map_err(|_| ETXTBSY)?;
        Ok(WriteAccess(self.clone()))
    }
    /// `deny_write_access` (`atomic_dec_unless_positive`): ETXTBSY while the file is open for writing.
    pub fn deny_write(self: &Arc<Self>) -> Result<DenyWrite, Errno> {
        self.write_count.try_update(Ordering::AcqRel, Ordering::Acquire, |v| (v <= 0).then_some(v - 1)).map_err(|_| ETXTBSY)?;
        Ok(DenyWrite(self.clone()))
    }
    pub fn readlink(&self) -> Result<Vec<u8>, Errno> {
        if let Some(m) = &self.magic {
            m.may_read()?;
        }
        match &self.data {
            Data::Symlink(t) => Ok(t.clone()),
            _ => Err(EINVAL),
        }
    }
    /// stat without the mount fields; `dir_entries` is the entry count for a directory (tmpfs reports
    /// `BOGO_DIRENT_SIZE` (20) per entry plus two for the dots as the size).
    pub fn attr(&self, dev: u64, dir_entries: usize) -> Attr {
        let t = *self.meta.times.lock();
        // tmpfs reports BOGO_DIRENT_SIZE per entry; proc and sysfs directories are 0 bytes.
        let size = match &self.data {
            Data::Dir => 20 * (dir_entries as u64 + 2),
            _ => self.size(),
        };
        let rdev = match &self.data {
            Data::Device(f) => f.rdev(),
            _ => 0,
        };
        Attr {
            dev,
            ino: self.meta.ino,
            mode: self.meta.mode(),
            nlink: self.meta.nlink() + match &self.data { Data::SynthDir(d) => d.extra_nlink(), _ => 0 },
            uid: self.meta.uid(),
            gid: self.meta.gid(),
            rdev,
            size,
            blksize: PAGE as u32,
            blocks: self.blocks(),
            atime: t.atime,
            mtime: t.mtime,
            ctime: t.ctime,
            btime: t.btime,
            mnt_id: 0,
            mount_root: false,
        }
    }
    pub fn read_at(&self, pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        match &self.data {
            Data::Reg(c) => c.read_at(pos, buf),
            Data::External(o) => o.read_at(pos, buf),
            Data::Dir | Data::SynthDir(_) => Err(EISDIR),
            _ => Err(EINVAL),
        }
    }
    /// Writes at `pos`; the caller checked size limits and freeze state.
    pub fn write_at(&self, pos: u64, buf: &[u8]) -> Result<usize, Errno> {
        let r = match &self.data {
            Data::Reg(c) => c.write_at(pos, buf),
            Data::External(o) => o.write_at(pos, buf),
            Data::Dir | Data::SynthDir(_) => return Err(EISDIR),
            _ => return Err(EINVAL),
        };
        if let Ok(n) = r
            && n > 0
        {
            self.meta.touch_mtime();
        }
        r
    }
    /// Appends: the end is taken under the content lock, so concurrent appenders never overwrite each other.
    pub fn append(&self, buf: &[u8]) -> Result<(usize, u64), Errno> {
        let r = match &self.data {
            Data::Reg(c) => c.append(buf, abi::MAX_FILE_SIZE),
            Data::External(o) => {
                let end = o.size();
                o.write_at(end, buf).map(|w| (w, end + w as u64))
            }
            Data::Dir | Data::SynthDir(_) => return Err(EISDIR),
            _ => return Err(EINVAL),
        };
        if let Ok((n, _)) = r
            && n > 0
        {
            self.meta.touch_mtime();
        }
        r
    }
    /// Sets the size: growing a tmpfs file charges its pool first (ENOMEM/ENOSPC, nothing changed).
    pub fn truncate(&self, size: u64) -> Result<(), Errno> {
        if size > abi::MAX_FILE_SIZE {
            return Err(EFBIG);
        }
        // MM-rmap: a shrink first takes the cut-off page-cache pages out of every mapping, so the cut can free
        // them, and after a successful cut again with the private copies and the pages a fault mapped in between
        // (`shmem_setattr`, linux-6.6/mm/shmem.c:1191-1202: unmap, truncate, "unmap again to remove racily COWed
        // private pages"; even_cows = 1), so a later touch past the end gives SIGBUS. The first round keeps the
        // private copies (even_cows = 0): a cut that fails (EIO, EROFS from a file system) only costs refaults,
        // never a program's private data (Linux checks before it unmaps).
        let cut = self.cut_from(size);
        if let Some(first) = cut {
            crate::mmap::unmap_mapping(self, first, u64::MAX, false);
        }
        let r = self.truncate_data(size);
        if let (Some(first), Ok(())) = (cut, &r) {
            crate::mmap::unmap_mapping(self, first, u64::MAX, true);
        }
        r
    }
    /// The first file page a truncate to `size` cuts off while a mapping may map it (None: nothing is cut, or
    /// a seal refuses the shrink, so no mapping loses its pages for a call that fails).
    fn cut_from(&self, size: u64) -> Option<u64> {
        let c = self.page_cache()?;
        let old = c.size();
        let first = size.div_ceil(PAGE);
        if size >= old || first >= old.div_ceil(PAGE) {
            return None;
        }
        if c.seals().unwrap_or(0) & crate::memfd::F_SEAL_SHRINK != 0 {
            return None;
        }
        Some(first)
    }
    fn truncate_data(&self, size: u64) -> Result<(), Errno> {
        match &self.data {
            Data::Reg(c) => {
                if let Err(e) = c.truncate(size) {
                    // EPERM from a tmpfs file's truncate is always a seal (F_SEAL_SHRINK/F_SEAL_GROW).
                    if e == EPERM {
                        crate::memfd::seal_refused(crate::memfd::Refused::Truncate, self.meta.ino, c.seals().unwrap_or(0), e);
                    }
                    return Err(e);
                }
            }
            Data::External(o) => o.truncate(size)?,
            Data::Dir | Data::SynthDir(_) => return Err(EISDIR),
            // O_TRUNC on a proc/sys attribute (`echo x > /proc/...`): nothing to cut, as Linux's proc_setattr.
            Data::Synth(_) | Data::Proc(_) => return Ok(()),
            _ => return Err(EINVAL),
        }
        self.meta.touch_mtime();
        Ok(())
    }
    /// fallocate after `vfs_fallocate`'s checks: tmpfs files (`PageCache::fallocate`, `shmem_fallocate`; success
    /// changes mtime and ctime even when nothing grew, `file_modified`), a disk file system's own
    /// (`InodeOps::fallocate`, FS-1b: vfat); everything else EOPNOTSUPP.
    pub fn fallocate(&self, mode: u32, offset: u64, len: u64) -> Result<(), Errno> {
        if let Data::External(o) = &self.data {
            if o.fallocate(mode, offset, len)? {
                self.meta.touch_mtime();
            }
            return Ok(());
        }
        let Data::Reg(c) = &self.data else { return Err(EOPNOTSUPP) };
        // MM-rmap: a hole punch first takes the whole pages of the hole out of every mapping (`shmem_fallocate`,
        // linux-6.6/mm/shmem.c:3061, even_cows = 0: private copies keep their data), so the cache can free them;
        // partly punched pages are zeroed in place. Not for a mode the cache refuses (vfs_fallocate's own checks,
        // `sys::fallocate`, refuse PUNCH_HOLE without KEEP_SIZE before this), nor when the seals refuse the punch:
        // a refused call keeps every mapping as it was.
        if mode & abi::FALLOC_FL_PUNCH_HOLE != 0
            && mode & !(abi::FALLOC_FL_PUNCH_HOLE | abi::FALLOC_FL_KEEP_SIZE) == 0
            && c.seals().unwrap_or(0) & (crate::memfd::F_SEAL_WRITE | crate::memfd::F_SEAL_FUTURE_WRITE) == 0
            && let Some(end) = offset.checked_add(len)
        {
            let (first, past) = (offset.div_ceil(PAGE), end / PAGE);
            if past > first {
                crate::mmap::unmap_mapping(self, first, past - 1, false);
            }
        }
        if let Err(e) = c.fallocate(mode, offset, len) {
            if e == EPERM {
                crate::memfd::seal_refused(crate::memfd::Refused::Fallocate, self.meta.ino, c.seals().unwrap_or(0), e);
            }
            return Err(e);
        }
        self.meta.touch_mtime();
        Ok(())
    }
    pub fn seek_data(&self, pos: u64, hole: bool) -> Option<u64> {
        match &self.data {
            Data::Reg(c) => c.seek_data(pos, hole),
            _ => {
                let size = self.size();
                if pos >= size {
                    None
                } else if hole {
                    Some(size)
                } else {
                    Some(pos)
                }
            }
        }
    }
    /// fsync/fdatasync: the dirty pages of its page cache go back to the file system (tmpfs: nothing to do).
    pub fn sync(&self, why: crate::pagecache::SyncKind) -> Result<(), Errno> {
        if let Data::External(o) = &self.data {
            return o.sync(why);
        }
        match self.page_cache() {
            Some(c) => c.sync_all(why),
            None => Ok(()),
        }
    }
}

/// `generic_write_check_limits`: a write starting at or past the largest offset is EFBIG.
pub fn check_limit(pos: u64, _len: usize) -> Result<(), Errno> {
    if pos >= abi::MAX_FILE_SIZE { Err(EFBIG) } else { Ok(()) }
}

/// Inodes ever created (for the `vfs root.ready` event and tests).
pub fn allocated() -> u64 {
    ALLOCATED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sparse_pages() {
        let p = PageCache::tmpfs(Arc::new(crate::pool::Pool::new("test")));
        assert_eq!(p.write_at(10_000, b"abc").unwrap(), 3);
        assert_eq!(p.size(), 10_003);
        assert_eq!(p.pages(), 1);
        assert_eq!(p.charged(), 1, "FS-14: only the allocated page is charged, holes are free");
        let mut b = [7u8; 8];
        assert_eq!(p.read_at(9_998, &mut b).unwrap(), 5);
        assert_eq!(&b[..5], &[0, 0, b'a', b'b', b'c']);
        assert_eq!(p.seek_data(0, false), Some(8192));
        assert_eq!(p.seek_data(0, true), Some(0));
        assert_eq!(p.seek_data(8192, true), Some(10_003));
        p.truncate(10_001).unwrap();
        p.truncate(10_003).unwrap();
        assert_eq!(p.read_at(10_000, &mut b).unwrap(), 3);
        assert_eq!(&b[..3], &[b'a', 0, 0]);
        p.truncate(0).unwrap();
        assert_eq!((p.pages(), p.charged()), (0, 0));
        // A write far out stays sparse: one page, one page charged (FS-14, Linux VM_NORESERVE).
        p.write_at(1 << 40, b"x").unwrap();
        assert_eq!(p.pages(), 1);
        assert_eq!(p.charged(), 1);
        // A size-charged object (shared anonymous memory) is charged for every page its size covers.
        let a = PageCache::tmpfs_accounted(Arc::new(crate::pool::Pool::new("test")));
        a.truncate(3 * PAGE).unwrap();
        assert_eq!((a.pages(), a.charged()), (0, 3));
    }
    #[test]
    fn dac() {
        let m = Meta::new(5, uapi::S_IFREG | 0o640, Cred { uid: 10, gid: 20 });
        assert!(m.permission(Cred { uid: 10, gid: 1 }, MAY_READ | MAY_WRITE).is_ok());
        assert_eq!(m.permission(Cred { uid: 11, gid: 20 }, MAY_WRITE), Err(EACCES));
        assert!(m.permission(Cred { uid: 11, gid: 20 }, MAY_READ).is_ok());
        assert_eq!(m.permission(Cred { uid: 11, gid: 21 }, MAY_READ), Err(EACCES));
        assert!(m.permission(Cred::ROOT, MAY_READ | MAY_WRITE).is_ok());
        assert_eq!(m.permission(Cred::ROOT, MAY_EXEC), Err(EACCES));
    }
}
