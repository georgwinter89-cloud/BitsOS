//! The two fd types of DR-3: a sync_file (one fence as an fd, linux-6.6/drivers/dma-buf/sync_file.c: poll,
//! SYNC_IOC_MERGE, SYNC_IOC_FILE_INFO, and SYNC_IOC_SET_DEADLINE of newer Linux) and a syncobj file
//! (`anon_inode:syncobj_file`, drm_syncobj.c:664, no operations: it only carries the object to another
//! process or file). Both are opened read-only and close-on-exec, as Linux's `anon_inode_getfile(.., 0)`.
//!
//! The VFS has no downcast for `dyn File`; the registry below finds the object behind an fd by its address
//! (the address is only compared, never dereferenced).

use crate::fence::{self, Fence, FenceCb, Registration};
use crate::syncobj::Syncobj;
use crate::uapi::{sync_fence_info, sync_file_info, sync_merge_data, sync_set_deadline, SYNC_IOC_FILE_INFO, SYNC_IOC_MERGE, SYNC_IOC_SET_DEADLINE};
use crate::wire::Wire;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::Write;
use ksync::SpinLock;
use linux::errno::*;
use vfs::fd::{FdTable, OpenFile};
use vfs::poll::{Events, PollQueue};
use vfs::sys::UserMem;
use vfs::{Errno, File};

klog::counter!(SYNC_FILES, "drm.sync_files.created");

enum Entry {
    Obj(Weak<SyncobjFile>),
    Sync(Weak<SyncFile>),
}

static FILES: SpinLock<BTreeMap<usize, Entry>> = SpinLock::named("drm.sync_file.registry", BTreeMap::new());

fn addr_of(f: &Arc<dyn File>) -> usize {
    Arc::as_ptr(f) as *const () as usize
}

fn addr<T>(t: &T) -> usize {
    t as *const T as *const () as usize
}

/// The sync_file behind an open file (`sync_file_fdget`), None for any other file.
pub fn as_sync_file(f: &Arc<dyn File>) -> Option<Arc<SyncFile>> {
    let a = addr_of(f);
    let s = match FILES.lock().get(&a) {
        Some(Entry::Sync(w)) => w.upgrade()?,
        _ => return None,
    };
    (addr(&*s) == a).then_some(s)
}

/// The syncobj behind a syncobj file (`f_op == &drm_syncobj_file_fops`).
pub fn as_syncobj(f: &Arc<dyn File>) -> Option<Arc<Syncobj>> {
    let a = addr_of(f);
    let s = match FILES.lock().get(&a) {
        Some(Entry::Obj(w)) => w.upgrade()?,
        _ => return None,
    };
    (addr(&*s) == a).then(|| s.obj.clone())
}

/// The fd table of the calling program (None in kernel threads).
pub fn current_fds() -> Option<Arc<FdTable>> {
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        proc::current().map(|p| p.fds.clone())
    }
}

/// `get_unused_fd_flags(O_CLOEXEC)` + `fd_install` of a read-only anonymous file. EMFILE without a table.
pub fn install(fds: Option<&FdTable>, f: Arc<dyn File>) -> Result<i32, Errno> {
    let fds = fds.ok_or(EMFILE)?;
    let n = fds.install_flags(OpenFile::anon(f, vfs::abi::O_RDONLY), 0, true)?;
    i32::try_from(n).map_err(|_| EMFILE)
}

/// The open file of an fd: Err(EBADF) for a negative or closed one.
pub fn lookup(fds: Option<&FdTable>, fd: i32) -> Result<Arc<dyn File>, Errno> {
    let fds = fds.ok_or(EBADF)?;
    let n = usize::try_from(fd).map_err(|_| EBADF)?;
    Ok(fds.get(n)?.file.clone())
}

// ---- syncobj file

pub struct SyncobjFile {
    obj: Arc<Syncobj>,
}

impl SyncobjFile {
    pub fn new(obj: Arc<Syncobj>) -> Arc<SyncobjFile> {
        let f = Arc::new(SyncobjFile { obj });
        FILES.lock().insert(addr(&*f), Entry::Obj(Arc::downgrade(&f)));
        f
    }
}

impl Drop for SyncobjFile {
    fn drop(&mut self) {
        FILES.lock().remove(&addr(self));
    }
}

impl File for SyncobjFile {
    /// No read in `drm_syncobj_file_fops`: EINVAL (`vfs_read`).
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    /// Anonymous inodes have no type bits.
    fn mode(&self) -> u32 {
        0o600
    }
    /// The anonymous inode every anon fd shares (`anon_inode_getfile`), as eventfd's.
    fn attr(&self) -> Option<vfs::abi::Attr> {
        Some(vfs::eventfd::anon_attr())
    }
    fn ioctl_user(&self, _cmd: u64, _arg: u64, _mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        Some(Err(ENOTTY))
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:syncobj_file"))
    }
}

// ---- sync_file

pub struct SyncFile {
    fence: Arc<Fence>,
    /// `user_name` (merge's name); empty = the name made from the fence.
    name: [u8; 32],
    poll: PollQueue,
    reg: SpinLock<Option<Registration>>,
}

/// Wakes the sync_file's pollers when its fence may be signaled (`fence_check_cb_func`).
struct PollCb(Weak<SyncFile>);

impl FenceCb for PollCb {
    fn call(&self) {
        if let Some(s) = self.0.upgrade() {
            if fence::is_signaled(&s.fence) {
                s.poll.wake_poll(Events::IN);
            }
        }
    }
}

impl SyncFile {
    /// `sync_file_create`. The poll callback is armed at once (Linux arms it at the first poll): the state
    /// changes before the wake, and a fence signaled before arming is seen by `events()` (FS-17).
    pub fn new(fence: Arc<Fence>, name: &[u8]) -> Arc<SyncFile> {
        let mut n = [0u8; 32];
        let len = name.iter().position(|b| *b == 0).unwrap_or(name.len()).min(31);
        n[..len].copy_from_slice(&name[..len]);
        let s = Arc::new(SyncFile { fence, name: n, poll: PollQueue::new(), reg: SpinLock::named("drm.sync_file.reg", None) });
        FILES.lock().insert(addr(&*s), Entry::Sync(Arc::downgrade(&s)));
        let reg = fence::add_callback(&s.fence, Arc::new(PollCb(Arc::downgrade(&s))));
        *s.reg.lock() = reg;
        SYNC_FILES.inc();
        s
    }

    pub fn fence(&self) -> &Arc<Fence> {
        &self.fence
    }

    /// `sync_file_get_name`: the merge's name, else `<driver>-<timeline><context>-<seqno>` (31 bytes at most).
    pub fn name(&self) -> [u8; 32] {
        if self.name[0] != 0 {
            return self.name;
        }
        let f = &self.fence;
        let mut s = String::new();
        let _ = write!(s, "{}-{}{}-{}", f.driver_name(), f.timeline_name(), f.context(), f.seqno() as i64);
        let mut n = [0u8; 32];
        let len = s.len().min(31);
        n[..len].copy_from_slice(&s.as_bytes()[..len]);
        n
    }

    /// The ioctls with an explicit fd table (host tests); `ioctl_user` uses the caller's.
    pub fn ioctl_with(self: &Arc<Self>, cmd: u32, arg: u64, mem: &dyn UserMem, fds: Option<&FdTable>) -> Result<i64, Errno> {
        match cmd {
            SYNC_IOC_MERGE => self.merge(arg, mem, fds),
            SYNC_IOC_FILE_INFO => self.info(arg, mem),
            SYNC_IOC_SET_DEADLINE => {
                let mut b = [0u8; sync_set_deadline::SIZE];
                if !mem.read(arg, &mut b) {
                    return Err(EFAULT);
                }
                let d = sync_set_deadline::decode(&b).ok_or(EFAULT)?;
                if d.pad != 0 {
                    return Err(EINVAL);
                }
                // A hint for the driver's clocks; the fake has none.
                Ok(0)
            }
            _ => Err(ENOTTY),
        }
    }

    /// `sync_file_ioctl_merge` (sync_file.c:207).
    fn merge(self: &Arc<Self>, arg: u64, mem: &dyn UserMem, fds: Option<&FdTable>) -> Result<i64, Errno> {
        let mut b = [0u8; sync_merge_data::SIZE];
        if !mem.read(arg, &mut b) {
            return Err(EFAULT);
        }
        let mut d = sync_merge_data::decode(&b).ok_or(EFAULT)?;
        if d.flags != 0 || d.pad != 0 {
            return Err(EINVAL);
        }
        let other = lookup(fds, d.fd2).ok().and_then(|f| as_sync_file(&f)).ok_or(ENOENT)?;
        d.name[31] = 0;
        let merged = SyncFile::new(fence::merge(&[&self.fence, &other.fence]), &d.name);
        // Linux order: an unused fd, the copy back, then `fd_install`; a fault gives the number back unused.
        let t = fds.ok_or(EMFILE)?;
        let n = t.reserve(0, true)?;
        let fd = match i32::try_from(n) {
            Ok(fd) => fd,
            Err(_) => {
                t.unreserve(n);
                return Err(EMFILE);
            }
        };
        d.fence = fd;
        if d.encode(&mut b).is_none() || !mem.write(arg, &b) {
            t.unreserve(n);
            return Err(EFAULT);
        }
        t.commit(n, OpenFile::anon(merged, vfs::abi::O_RDONLY));
        Ok(0)
    }

    /// `sync_file_ioctl_fence_info` (sync_file.c:279).
    fn info(&self, arg: u64, mem: &dyn UserMem) -> Result<i64, Errno> {
        let mut b = [0u8; sync_file_info::SIZE];
        if !mem.read(arg, &mut b) {
            return Err(EFAULT);
        }
        let mut info = sync_file_info::decode(&b).ok_or(EFAULT)?;
        if info.flags != 0 || info.pad != 0 {
            return Err(EINVAL);
        }
        let fences = fence::unwrap(&self.fence);
        let num = u32::try_from(fences.len()).map_err(|_| ENOMEM)?;
        if info.num_fences == 0 {
            info.status = fence::status(&self.fence);
        } else {
            info.status = 1;
            if info.num_fences < num {
                return Err(EINVAL);
            }
            let mut out: Vec<u8> = Vec::new();
            out.try_reserve_exact(fences.len() * sync_fence_info::SIZE).map_err(|_| ENOMEM)?;
            out.resize(fences.len() * sync_fence_info::SIZE, 0);
            for (k, f) in fences.iter().enumerate() {
                let mut fi = sync_fence_info::ZERO;
                copy_name(&mut fi.obj_name, f.timeline_name());
                copy_name(&mut fi.driver_name, f.driver_name());
                fi.status = fence::status(f);
                fi.timestamp_ns = if fence::is_signaled(f) { f.timestamp() } else { 0 };
                fi.encode(&mut out[k * sync_fence_info::SIZE..]).ok_or(EFAULT)?;
                if info.status > 0 {
                    info.status = fi.status;
                }
            }
            if !out.is_empty() && !mem.write(info.sync_fence_info, &out) {
                return Err(EFAULT);
            }
        }
        info.name = self.name();
        info.num_fences = num;
        info.encode(&mut b).ok_or(EFAULT)?;
        if !mem.write(arg, &b) {
            return Err(EFAULT);
        }
        Ok(0)
    }
}

/// `strscpy` into a zeroed field.
fn copy_name(dst: &mut [u8; 32], s: &str) {
    let n = s.len().min(31);
    dst[..n].copy_from_slice(&s.as_bytes()[..n]);
}

impl Drop for SyncFile {
    fn drop(&mut self) {
        FILES.lock().remove(&addr(self));
        drop(self.reg.get_mut().take());
    }
}

impl File for SyncFile {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        0o600
    }
    /// The anonymous inode every anon fd shares (`anon_inode_getfile`), as eventfd's.
    fn attr(&self) -> Option<vfs::abi::Attr> {
        Some(vfs::eventfd::anon_attr())
    }
    /// `sync_file_poll`: EPOLLIN once the fence is signaled, computed now.
    fn events(&self) -> Events {
        Events(if fence::is_signaled(&self.fence) { Events::IN } else { 0 })
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.poll)
    }
    fn can_poll(&self) -> bool {
        true
    }
    fn poll_exact(&self) -> bool {
        true
    }
    fn link_name(&self) -> Option<String> {
        Some(String::from("anon_inode:sync_file"))
    }
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        // The registry gives the Arc back (merge needs it for the new file's fence).
        let me = match FILES.lock().get(&addr(self)) {
            Some(Entry::Sync(w)) => w.upgrade(),
            _ => None,
        }?;
        let fds = current_fds();
        Some(me.ioctl_with(cmd as u32, arg, mem, fds.as_deref()))
    }
}
