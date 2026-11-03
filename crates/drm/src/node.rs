//! The device nodes: `/dev/dri/renderD<minor>` (char 226:minor, 0666 as CachyOS's render node; V2 has no
//! `render` group) and the open DRM file behind each open (Linux `drm_open`/`drm_release`, drm_file.c:409/483).
//! A node exists only while its device is registered (`drm::add_device`/`remove_device`), as on Linux.

use crate::device::{self, Device, DRM_MAJOR};
use crate::dispatch::{self, Caller, Ctx};
use crate::file::{DrmFile, MinorKind};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::fmt::Write;
use linux::errno::*;
use vfs::poll::{Events, PollQueue};
use vfs::sys::UserMem;
use vfs::{Errno, File};

const MODE: u32 = linux::S_IFCHR | 0o666;

/// The node object of a render minor: each open finds the device registered under the minor now.
pub struct MinorNode {
    minor: u32,
}

impl MinorNode {
    pub fn new(minor: u32) -> MinorNode {
        MinorNode { minor }
    }
}

fn caller() -> Caller {
    match proc::current() {
        Some(p) => Caller { pid: p.pid, sys_admin: p.capable(proc::cred::Cap::SysAdmin), sys_nice: p.capable(proc::cred::Cap::SysNice) },
        // A kernel thread (ktests): the kernel may do everything.
        None => Caller { pid: 0, sys_admin: true, sys_nice: true },
    }
}

/// `drm_open`: ENODEV when no device has the minor (Linux `drm_minor_acquire`).
pub fn open(minor: u32) -> Result<Arc<OpenDrm>, Errno> {
    let Some(dev) = device::by_render_minor(minor) else {
        klog::event!(Debug, "drm", "open.no_device", minor = minor; "ENODEV");
        return Err(ENODEV);
    };
    open_device(dev, MinorKind::Render, caller().sys_admin)
}

pub fn open_device(dev: Arc<Device>, kind: MinorKind, sys_admin: bool) -> Result<Arc<OpenDrm>, Errno> {
    let Some(inside) = dev.enter() else {
        klog::event!(Debug, "drm", "open.unplugged", minor = dev.render_minor(); "ENODEV");
        return Err(ENODEV);
    };
    let file = DrmFile::new(kind, sys_admin);
    if let Err(e) = dev.driver().open(&file) {
        klog::event!(Info, "drm", "open.failed", minor = dev.render_minor(), errno = e; "the driver refused the open");
        return Err(e);
    }
    drop(inside);
    Ok(Arc::new(OpenDrm { dev, file, poll: PollQueue::new() }))
}

impl File for MinorNode {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> Result<usize, Errno> {
        Err(ENODEV)
    }
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(ENODEV)
    }
    fn mode(&self) -> u32 {
        MODE
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(DRM_MAJOR, self.minor)
    }
    fn open_node(&self, _flags: u64, _who: vfs::inode::Cred) -> Option<Result<Arc<dyn File>, Errno>> {
        Some(open(self.minor).map(|f| f as Arc<dyn File>))
    }
}

/// One open of a DRM node (Linux's `struct file` with its `drm_file`). Dropped at the last close.
pub struct OpenDrm {
    dev: Arc<Device>,
    file: DrmFile,
    /// No events before KMS; the queue is there so poll/epoll can wait on the fd as on Linux.
    poll: PollQueue,
}

impl OpenDrm {
    pub fn device(&self) -> &Arc<Device> {
        &self.dev
    }
    pub fn drm_file(&self) -> &DrmFile {
        &self.file
    }
    /// The ioctl with an explicit caller (ktests and host tests); fds of the calling program, if any.
    pub fn ioctl_as(&self, cmd: u32, arg: u64, mem: &dyn UserMem, caller: Caller) -> Result<i64, Errno> {
        self.ioctl_with(cmd, arg, mem, caller, crate::sync_file::current_fds())
    }
    /// CR-DR6-1: the mapping of the GEM object at `pgoff` for `pages` (`drm_gem_mmap`'s checks: EINVAL, EACCES,
    /// EPERM). ENODEV for a device without the GEM uAPI.
    pub fn object_map(&self, pgoff: u64, pages: u64) -> Result<Arc<mm::device::DeviceMap>, Errno> {
        let gpu = self.dev.driver().gpu().ok_or(ENODEV)?;
        crate::gem_map::object_map(&self.file, &gpu.shared().gem, pgoff, pages)
    }
    /// The ioctl with an explicit caller and fd table (the fds that HANDLE_TO_FD makes and FD_TO_HANDLE reads).
    pub fn ioctl_with(&self, cmd: u32, arg: u64, mem: &dyn UserMem, caller: Caller, fds: Option<Arc<vfs::fd::FdTable>>) -> Result<i64, Errno> {
        let ctx = Ctx { dev: &self.dev, file: &self.file, caller, mem, fds };
        dispatch::ioctl(&ctx, cmd, arg)
    }
}

impl Drop for OpenDrm {
    /// `drm_release` -> `drm_file_free`: the driver's postclose, then every handle and syncobj handle.
    fn drop(&mut self) {
        self.dev.driver().postclose(&self.file);
        let c = crate::ctx::release_file(&self.dev, &self.file);
        crate::bo_list::release_file(&self.file);
        let n = self.file.release_handles();
        crate::gem::release_file(&self.file);
        crate::vm_ioctl::release_file(&self.dev, &self.file);
        let s = self.file.release_syncobjs();
        if n + s + c > 0 {
            klog::event!(Debug, "drm", "file.released", minor = self.dev.render_minor(), client = self.file.client_id(), handles = n, syncobjs = s, ctxs = c);
        }
    }
}

impl File for OpenDrm {
    /// `drm_read`: no events exist before KMS. Linux would wait for one; an fd without any event source answers
    /// EAGAIN instead of waiting for ever (no program reads a render node).
    fn read_at(&self, _pos: u64, buf: &mut [u8]) -> Result<usize, Errno> {
        if buf.is_empty() {
            return Ok(0);
        }
        Err(EAGAIN)
    }
    /// The DRM fops have no write: EINVAL (`vfs_write`).
    fn write_at(&self, _pos: u64, _buf: &[u8]) -> Result<usize, Errno> {
        Err(EINVAL)
    }
    fn mode(&self) -> u32 {
        MODE
    }
    fn rdev(&self) -> u64 {
        vfs::abi::encode_dev(DRM_MAJOR, self.dev.render_minor())
    }
    /// `drm_poll`: readable only with an event pending (none yet).
    fn events(&self) -> Events {
        Events(0)
    }
    fn poll_head(&self) -> Option<&PollQueue> {
        Some(&self.poll)
    }
    /// CR-DR6-1: the GEM object at `pgoff` alone (`drm_gem_mmap`); devices without the GEM uAPI as before.
    fn device_map_at(&self, pgoff: u64, pages: u64) -> Option<Result<Arc<dyn core::any::Any + Send + Sync>, Errno>> {
        self.dev.driver().gpu()?;
        Some(self.object_map(pgoff, pages).map(|m| m as Arc<dyn core::any::Any + Send + Sync>))
    }
    /// DR-6: the GEM objects' memory at their fake offsets (see `gem_map`).
    fn device_map(&self) -> Option<Arc<dyn core::any::Any + Send + Sync>> {
        let gpu = self.dev.driver().gpu()?;
        crate::gem_map::file_map(&self.file, &gpu.shared().gem).map(|m| m as Arc<dyn core::any::Any + Send + Sync>)
    }
    fn show_fdinfo(&self, out: &mut String) {
        let p = self.dev.driver().pci();
        let _ = write!(out, "drm-driver:\t{}\ndrm-client-id:\t{}\ndrm-pdev:\t{}\n", self.dev.driver().info().name, self.file.client_id(), p.slot_name());
        self.dev.driver().fdinfo(&self.file, out);
    }
    /// Linux takes `unsigned int cmd`: the upper half of the register is not part of the number.
    fn ioctl_user(&self, cmd: u64, arg: u64, mem: &dyn UserMem) -> Option<Result<i64, Errno>> {
        Some(self.ioctl_as(cmd as u32, arg, mem, caller()))
    }
}

/// The node of a registered device: `/dev/dri/renderD<minor>` (missing directories made, an old node replaced).
pub fn publish(dev: &Device) {
    let minor = dev.render_minor();
    let path = format!("/dev/dri/renderD{minor}");
    match vfs::root_ns().create_device(&path, Arc::new(MinorNode::new(minor))) {
        Ok(_) => klog::event!(Info, "drm", "node.created", path = path.as_str()),
        Err(e) => klog::event!(Warn, "drm", "node.failed", path = path.as_str(), errno = e),
    }
}

/// The node goes with its device (/dev/dri stays, as with devtmpfs).
pub fn unpublish(dev: &Device) {
    let path = format!("/dev/dri/renderD{}", dev.render_minor());
    match vfs::root_ns().remove(&path) {
        Ok(()) | Err(ENOENT) => {}
        Err(e) => klog::event!(Warn, "drm", "node.remove_failed", path = path.as_str(), errno = e),
    }
}
