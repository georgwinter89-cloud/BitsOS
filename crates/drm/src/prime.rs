//! DR-6: PRIME, GEM objects as dma-bufs and back (linux-6.6/drivers/gpu/drm/drm_prime.c:295-523,
//! amd/amdgpu/amdgpu_dma_buf.c:288-470).
//!
//! - HANDLE_TO_FD: flags CLOEXEC|RDWR only (EINVAL), ENOENT for an unknown handle, the file's PRIME cache first,
//!   then the buffer the object already is (an import's original, or its earlier export while a handle exists),
//!   else a new export (`amdgpu_gem_prime_export`: EPERM for VM_ALWAYS_VALID objects) that shares the object's
//!   reservation object. One open file per buffer (`DmaBuf::install_mode`).
//! - FD_TO_HANDLE: EBADF for a closed fd, EINVAL for an fd that is no dma-buf; the file's cache gives the same
//!   handle again; a buffer exported by this device gives its own object (`amdgpu_gem_prime_import`), anything
//!   else (udmabuf, another device) a new GTT object over the exporter's pages that shares the buffer's
//!   reservation object (`amdgpu_dma_buf_create_obj`).

use crate::dispatch::Ctx;
use crate::dma_buf::{self, DmaBuf, Exporter};
use crate::gem::{buf_key, Bo, NewBo, PAGE};
use crate::gpu::{BoPlace, GpuDevice};
use crate::sync_file;
use crate::uapi::*;
use crate::wire::Wire;
use alloc::sync::Arc;
use linux::errno::*;
use mm::device::Aperture;
use vfs::Errno;

/// The exporter of a GEM object's dma-buf (`amdgpu_dmabuf_ops`, priv = the object): the object's pages, its
/// CPU-access rule for mappings.
pub struct BoExporter {
    bo: Arc<Bo>,
}

impl Exporter for BoExporter {
    /// `drm_gem_prime_export`'s `KBUILD_MODNAME`: drm_prime.c is built into drm.ko, so "drm" (Linux 7.3 shows it).
    fn name(&self) -> &'static str {
        "drm"
    }
    fn size(&self) -> u64 {
        self.bo.size()
    }
    fn aperture(&self) -> Option<Aperture> {
        self.bo.memory().aperture()
    }
    /// `amdgpu_gem_object_mmap`: EPERM without CPU access; then the memory's own rule.
    fn mmap_check(&self, shared: bool) -> Result<(), Errno> {
        if !self.bo.cpu_access() {
            return Err(EPERM);
        }
        self.bo.memory().mmap_check(shared)
    }
    fn begin_cpu(&self, dir: dma_buf::Direction) -> Result<(), Errno> {
        self.bo.memory().begin_cpu(dir)
    }
    fn end_cpu(&self, dir: dma_buf::Direction) -> Result<(), Errno> {
        self.bo.memory().end_cpu(dir)
    }
    fn page_phys(&self, page: u64) -> Option<u64> {
        self.bo.memory().page_phys(page)
    }
    fn read(&self, off: u64, out: &mut [u8]) -> bool {
        self.bo.read(off, out)
    }
    fn write(&self, off: u64, data: &[u8]) -> bool {
        self.bo.write(off, data)
    }
    fn gem_bo(&self) -> Option<Arc<Bo>> {
        Some(self.bo.clone())
    }
    fn cpu_fault(&self) {
        self.bo.cpu_touched();
    }
}

impl Bo {
    /// The buffer the object is shared as (`drm_gem_prime_handle_to_fd` from `out_have_obj` on): its import's
    /// original, its earlier export, or a new one. ENOENT when no handle names the object any more (a racing
    /// GEM_CLOSE).
    pub(crate) fn export(self: &Arc<Self>) -> Result<Arc<DmaBuf>, Errno> {
        if let Some(b) = &self.import {
            return Ok(b.clone());
        }
        {
            let g = self.st.lock();
            if g.handles == 0 {
                return Err(ENOENT);
            }
            if let Some(b) = &g.dma_buf {
                return Ok(b.clone());
            }
        }
        if self.flags() & AMDGPU_GEM_CREATE_VM_ALWAYS_VALID as u64 != 0 || self.is_userptr() {
            return Err(EPERM);
        }
        let new = DmaBuf::export(Arc::new(BoExporter { bo: self.clone() }), Some(self.resv().clone()))?;
        let (b, lost) = {
            let mut g = self.st.lock();
            if g.handles == 0 {
                (None, Some(new))
            } else if let Some(b) = &g.dma_buf {
                // Another export won the race: its buffer is the object's.
                (Some(b.clone()), Some(new))
            } else {
                g.dma_buf = Some(new.clone());
                (Some(new), None)
            }
        };
        drop(lost);
        let b = b.ok_or(ENOENT)?;
        klog::event!(Debug, "drm", "prime.exported", size = self.size(), ino = b.ino());
        Ok(b)
    }

    /// Same device: the object was made by the GPU whose GEM state is `gpu`'s.
    pub(crate) fn same_device(&self, gpu: &dyn GpuDevice) -> bool {
        Arc::ptr_eq(&self.gem, &gpu.shared().gem)
    }
}

/// `amdgpu_dma_buf_create_obj` (amdgpu_dma_buf.c:340): a GTT object of the buffer's size over its pages, the
/// buffer's reservation object, the flags another amdgpu device's object had for its CPU view.
fn import_obj(gpu: &dyn GpuDevice, buf: &Arc<DmaBuf>) -> Result<Arc<Bo>, Errno> {
    let keep = (AMDGPU_GEM_CREATE_CPU_GTT_USWC | AMDGPU_GEM_CREATE_COHERENT | AMDGPU_GEM_CREATE_UNCACHED) as u64;
    let flags = buf.exporter().gem_bo().map(|o| o.flags() & keep).unwrap_or(0);
    let n = NewBo {
        userptr: None,
        size: buf.size(),
        page_align: 1,
        flags,
        resv: Some(buf.resv().clone()),
        mem: buf.exporter().clone(),
        place: Some(BoPlace::Gtt),
        import: Some(buf.clone()),
        preferred: AMDGPU_GEM_DOMAIN_GTT,
        allowed: AMDGPU_GEM_DOMAIN_GTT,
    };
    debug_assert!(n.size % PAGE == 0);
    Bo::new(&gpu.shared().gem, n)
}

/// `drm_prime_handle_to_fd_ioctl` (drm_prime.c:510).
pub fn handle_to_fd(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let mut a = drm_prime_handle::decode(data).ok_or(EINVAL)?;
    if a.flags & !(DRM_CLOEXEC | DRM_RDWR) != 0 {
        return Err(EINVAL);
    }
    let rdwr = a.flags & DRM_RDWR != 0;
    let cloexec = a.flags & DRM_CLOEXEC != 0;
    let fg = &ctx.file.gem;
    let buf = {
        let _p = fg.prime_lock.lock();
        let cached = fg.prime.lock().by_handle.get(&a.handle).cloned();
        match cached {
            Some(b) => b,
            None => {
                let obj = ctx.file.handle_lookup(a.handle).ok_or(ENOENT)?;
                // Only amdgpu objects exist on V2's devices; another driver's object cannot be shared.
                let bo = obj.into_bo().ok_or(EINVAL)?;
                let b = bo.export()?;
                // The cache entry only while the handle still names the object (a racing GEM_CLOSE).
                if ctx.file.bo(a.handle).is_some_and(|o| Arc::ptr_eq(&o, &bo)) {
                    let mut g = fg.prime.lock();
                    g.by_buf.insert(buf_key(&b), a.handle);
                    g.by_handle.insert(a.handle, b.clone());
                }
                b
            }
        }
    };
    a.fd = buf.install_mode(ctx.fds.as_deref(), cloexec, rdwr)?;
    a.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

/// `drm_prime_fd_to_handle_ioctl` (drm_prime.c:364) -> `drm_gem_prime_fd_to_handle` (:295).
pub fn fd_to_handle(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let mut a = drm_prime_handle::decode(data).ok_or(EINVAL)?;
    let f = sync_file::lookup(ctx.fds.as_deref(), a.fd)?;
    let buf = dma_buf::as_dma_buf(&f).ok_or(EINVAL)?;
    drop(f);
    let fg = &ctx.file.gem;
    let _p = fg.prime_lock.lock();
    let cached = fg.prime.lock().by_buf.get(&buf_key(&buf)).copied();
    if let Some(h) = cached {
        a.handle = h;
        a.encode(data).ok_or(EINVAL)?;
        return Ok(0);
    }
    // `drm_gem_prime_import_dev`: a device without the GEM uAPI imports nothing.
    let gpu = ctx.dev.driver().gpu().ok_or(EINVAL)?;
    let bo = match buf.exporter().gem_bo() {
        Some(o) if o.same_device(gpu) => o,
        _ => import_obj(gpu, &buf)?,
    };
    // `amdgpu_gem_object_open`: an always-valid object lives in its own file's VM only.
    if bo.flags() & AMDGPU_GEM_CREATE_VM_ALWAYS_VALID as u64 != 0 && !Arc::ptr_eq(bo.resv(), &fg.vm_resv) {
        return Err(EPERM);
    }
    crate::userptr::check_mm(ctx, &bo)?;
    let h = ctx.file.handle_create(bo.clone())?;
    {
        let mut g = bo.st.lock();
        if g.handles > 0 && g.dma_buf.is_none() && bo.import.is_none() {
            g.dma_buf = Some(buf.clone());
        }
    }
    {
        let mut g = fg.prime.lock();
        g.by_buf.insert(buf_key(&buf), h);
        g.by_handle.insert(h, buf.clone());
    }
    klog::event!(Debug, "drm", "prime.imported", handle = h, size = buf.size(), own = bo.import.is_none());
    a.handle = h;
    a.encode(data).ok_or(EINVAL)?;
    Ok(0)
}
