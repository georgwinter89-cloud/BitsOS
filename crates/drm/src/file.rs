//! One open DRM file (Linux `struct drm_file`, linux-6.6/drivers/gpu/drm/drm_file.c:150 `drm_file_alloc`): its
//! minor's kind, the client id, authentication, the GEM handle table and the syncobj handle table (DR-3,
//! `syncobj.rs`), the file's GEM state (DR-6, `gem::FileGem`: PRIME cache, pin owner, mapping view). Dropped at
//! the last close of the fd.

use crate::handles::Handles;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use ksync::SpinLock;
use linux::errno::{EINVAL, ENOMEM};
use vfs::Errno;

/// `enum drm_minor_type` (render minors are 128 + index, primary ones the index).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinorKind {
    Primary,
    Render,
}

/// A buffer object a handle names (amdgpu's are `gem::Bo`, DR-6).
pub trait GemObject: Send + Sync {
    fn size(&self) -> u64;
    /// The amdgpu buffer object behind the handle (None for other drivers' objects).
    fn into_bo(self: Arc<Self>) -> Option<Arc<crate::gem::Bo>> {
        None
    }
    /// A handle of some file now names the object (`drm_gem_object_handle_get`, before the table entry).
    fn handle_opened(&self) {}
    /// A handle went (`drm_gem_object_release_handle`); called outside the file's locks.
    fn handle_closed(&self) {}
}

/// Linux's `ident` in drm_file_alloc: client ids from 1, never reused while the kernel runs.
static CLIENT_IDS: AtomicU64 = AtomicU64::new(1);

pub struct DrmFile {
    kind: MinorKind,
    client_id: u64,
    authenticated: AtomicBool,
    master: AtomicBool,
    handles: SpinLock<Handles<Arc<dyn GemObject>>>,
    /// DR-3: the sync object handles (Linux's `syncobj_idr`, apart from the GEM handles).
    pub(crate) syncobjs: SpinLock<Handles<Arc<crate::syncobj::Syncobj>>>,
    /// DR-4: the amdgpu contexts (Linux's `amdgpu_fpriv.ctx_mgr`), ended by `ctx::release_file`.
    pub(crate) gpu_ctxs: SpinLock<Handles<Arc<crate::ctx::GpuCtx>>>,
    /// DR-6: PRIME cache, pin owner, VM reservation object, mapping view (`file_priv->prime`, `amdgpu_fpriv`).
    pub(crate) gem: crate::gem::FileGem,
    /// DR-7: the GPU VM (`amdgpu_fpriv.vm`): bo_vas, mappings, page-table jobs.
    pub vm: crate::vm::FileVm,
    /// DR-9: the BO lists (`amdgpu_fpriv.bo_list_handles`).
    pub(crate) bo_lists: crate::bo_list::FileBoLists,
}

impl DrmFile {
    /// `sys_admin`: the opener's CAP_SYS_ADMIN (Linux: `authenticated = capable(CAP_SYS_ADMIN)`).
    pub fn new(kind: MinorKind, sys_admin: bool) -> DrmFile {
        let gem = crate::gem::FileGem::new();
        let vm = crate::vm::FileVm::with_root(gem.vm_resv.clone());
        DrmFile {
            kind,
            client_id: CLIENT_IDS.fetch_add(1, Ordering::Relaxed),
            authenticated: AtomicBool::new(sys_admin),
            master: AtomicBool::new(false),
            handles: SpinLock::named("drm.file.handles", Handles::new()),
            syncobjs: SpinLock::named("drm.file.syncobjs", Handles::new()),
            gpu_ctxs: SpinLock::named("drm.file.ctxs", Handles::new()),
            gem,
            vm,
            bo_lists: crate::bo_list::FileBoLists::new(),
        }
    }

    pub fn kind(&self) -> MinorKind {
        self.kind
    }
    pub fn is_render(&self) -> bool {
        self.kind == MinorKind::Render
    }
    pub fn client_id(&self) -> u64 {
        self.client_id
    }
    pub fn authenticated(&self) -> bool {
        self.authenticated.load(Ordering::Acquire)
    }
    /// No master yet: KMS on a primary node brings SET_MASTER/DROP_MASTER.
    pub fn is_master(&self) -> bool {
        self.master.load(Ordering::Acquire)
    }

    /// `drm_gem_handle_create_tail`: a new handle for `obj`.
    pub fn handle_create(&self, obj: Arc<dyn GemObject>) -> Result<u32, Errno> {
        obj.handle_opened();
        self.handle_insert_opened(obj)
    }
    /// The table entry for an object whose `handle_opened` already ran (GEM_OPEN counts it under the name lock).
    pub(crate) fn handle_insert_opened(&self, obj: Arc<dyn GemObject>) -> Result<u32, Errno> {
        // DR-7: the bo_va first (`amdgpu_gem_object_open`), so a GEM_VA on the new handle always finds it.
        let bo = obj.clone().into_bo();
        if let Some(b) = &bo {
            self.vm.bo_open(b);
        }
        let r = self.handles.lock().insert(obj.clone());
        match r {
            Ok(_) => self.gem.changed(),
            Err(_) => {
                if let Some(b) = &bo {
                    self.vm.bo_close(b);
                }
                obj.handle_closed()
            }
        }
        r
    }
    pub fn handle_lookup(&self, h: u32) -> Option<Arc<dyn GemObject>> {
        self.handles.lock().get(h).cloned()
    }
    /// The amdgpu buffer object of handle `h` (`drm_gem_object_lookup` + `gem_to_amdgpu_bo`).
    pub fn bo(&self, h: u32) -> Option<Arc<crate::gem::Bo>> {
        self.handle_lookup(h)?.into_bo()
    }
    /// Every object the file has a handle for (the mapping view, DR-6); ENOMEM when the list does not fit.
    pub(crate) fn handle_objects(&self) -> Result<Vec<Arc<dyn GemObject>>, Errno> {
        let g = self.handles.lock();
        let mut v = Vec::new();
        v.try_reserve_exact(g.len()).map_err(|_| ENOMEM)?;
        v.extend(g.values().cloned());
        Ok(v)
    }
    /// The file has a handle for the object at `obj` (`drm_vma_node_is_allowed`), without allocating.
    pub(crate) fn has_object(&self, obj: *const ()) -> bool {
        self.handles.lock().values().any(|o| Arc::as_ptr(o) as *const () == obj)
    }
    /// `drm_gem_handle_delete`: EINVAL for a handle the file does not have. The object is released outside the
    /// lock, after its PRIME cache entry (`drm_gem_remove_prime_handles`).
    pub fn handle_delete(&self, h: u32) -> Result<(), Errno> {
        let obj = self.handles.lock().remove(h).ok_or(EINVAL)?;
        // Under the PRIME lock, as Linux's `drm_gem_remove_prime_handles`: an import or export of this file that
        // is going on either finishes its cache entry first (removed here) or sees the handle gone.
        let buf = {
            let _p = self.gem.prime_lock.lock();
            self.gem.prime_forget(h)
        };
        self.gem.changed();
        // DR-7: the bo_va and its mappings at the file's last handle of the object (`amdgpu_gem_object_close`).
        if let Some(b) = obj.clone().into_bo() {
            self.vm.bo_close(&b);
        }
        obj.handle_closed();
        drop((obj, buf));
        Ok(())
    }
    pub fn handle_count(&self) -> usize {
        self.handles.lock().len()
    }
    /// The last close (`drm_gem_release`): every handle goes; objects are released outside the lock.
    pub fn release_handles(&self) -> usize {
        let all = self.handles.lock().drain();
        let bufs = self.gem.prime_clear();
        self.gem.changed();
        for (_, o) in &all {
            o.handle_closed();
        }
        drop(bufs);
        all.len()
    }
}
