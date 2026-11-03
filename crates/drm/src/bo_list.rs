//! DR-8: what CS (and DR-9's BO_LIST ioctl) copy from the program: user memory in pieces with kvmalloc's limit,
//! records out of chunk data, and BO lists (`amdgpu_bo_list.c`). DR-9: `DRM_IOCTL_AMDGPU_BO_LIST` (create,
//! destroy, update; linux-6.6/drivers/gpu/drm/amd/amdgpu/amdgpu_bo_list.c:236) and the per-file table a CS
//! names a list from (`amdgpu_bo_list_get`, :174).

use crate::dispatch::Ctx;
use crate::file::DrmFile;
use crate::gem::Bo;
use crate::handles::Handles;
use crate::uapi::*;
use crate::wire::Wire;
use alloc::sync::Arc;
use alloc::vec::Vec;
use ksync::SpinLock;
use linux::errno::*;
use vfs::Errno;

/// A BO list (`struct amdgpu_bo_list`): its objects with their priorities, shared by the table and every CS
/// that uses it (Linux's kref); an UPDATE puts a new list under the handle, a CS keeps the one it took.
pub(crate) type BoList = Arc<Vec<(Arc<Bo>, u32)>>;

/// The file's BO lists (`fpriv->bo_list_handles` with `bo_list_lock`): handles from 1, the lowest free first
/// (`idr_alloc(.., 1, 0, ..)`), no fixed count (R1). The lock is a leaf: a list is cloned out and the lock
/// dropped before anything else is taken (no new lock order for CS).
pub struct FileBoLists {
    t: SpinLock<Handles<BoList>>,
}

impl Default for FileBoLists {
    fn default() -> Self {
        FileBoLists::new()
    }
}

impl FileBoLists {
    pub fn new() -> FileBoLists {
        FileBoLists { t: SpinLock::named("drm.file.bo_lists", Handles::new()) }
    }
    /// `amdgpu_bo_list_get`: the list under `h` (ENOENT).
    pub(crate) fn get(&self, h: u32) -> Result<BoList, Errno> {
        self.t.lock().get(h).cloned().ok_or(ENOENT)
    }
    pub fn len(&self) -> usize {
        self.t.lock().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// `amdgpu_bo_list_ioctl` as CachyOS's kernel has it (the ABI program shows it; Linux 6.6 read the entries
/// first for every operation): CREATE reads the entries (EINVAL for the entry-size rule, ENOMEM, EFAULT) and
/// answers a new handle; DESTROY reads nothing and never fails (the answer's handle is 0); UPDATE reads the
/// entries, then ENOENT for an unknown handle, else the handle comes back; anything else EINVAL at once. The
/// whole union is zeroed before the answer.
pub fn ioctl(ctx: &Ctx, data: &mut [u8]) -> Result<i64, Errno> {
    let a = drm_amdgpu_bo_list::decode(data).ok_or(EINVAL)?;
    let li = a.r#in();
    let lists = &ctx.file.bo_lists;
    let handle = match li.operation {
        AMDGPU_BO_LIST_OP_CREATE => {
            let es = bo_list_entries(ctx, &li)?;
            let l = Arc::new(bo_list_create(ctx, &es)?);
            let h = lists.t.lock().insert(l)?;
            klog::event!(Debug, "drm", "bo_list.created", handle = h, bos = es.len());
            h
        }
        AMDGPU_BO_LIST_OP_DESTROY => {
            let old = lists.t.lock().remove(li.list_handle);
            klog::event!(Debug, "drm", "bo_list.destroyed", handle = li.list_handle, found = old.is_some());
            drop(old);
            0
        }
        AMDGPU_BO_LIST_OP_UPDATE => {
            let es = bo_list_entries(ctx, &li)?;
            let l = Arc::new(bo_list_create(ctx, &es)?);
            let old = lists.t.lock().replace(li.list_handle, l);
            match old {
                Ok(old) => drop(old),
                Err(new) => {
                    drop(new);
                    klog::event!(Debug, "drm", "bo_list.update_unknown", handle = li.list_handle; "ENOENT");
                    return Err(ENOENT);
                }
            }
            li.list_handle
        }
        op => {
            klog::event!(Debug, "drm", "bo_list.bad_op", op = op; "EINVAL");
            return Err(EINVAL);
        }
    };
    let mut o = drm_amdgpu_bo_list::ZERO;
    o.set_out(&drm_amdgpu_bo_list_out { list_handle: handle, _pad: 0 });
    o.encode(data).ok_or(EINVAL)?;
    Ok(0)
}

/// The last close (`amdgpu_driver_postclose_kms`): every list goes, the objects' references with them
/// (dropped outside the lock).
pub fn release_file(file: &DrmFile) {
    let all = file.bo_lists.t.lock().drain();
    drop(all);
}

/// `AMDGPU_BO_LIST_MAX_PRIORITY` (amdgpu_bo_list.h).
const BO_MAX_PRIORITY: u32 = 32;
/// `kvmalloc` refuses more than `INT_MAX` bytes (mm/util.c): Linux then answers ENOMEM.
const KVMALLOC_MAX: u64 = i32::MAX as u64;
/// User memory is copied in pieces of this size, so a refused range costs no more than the piece before it.
const COPY_PIECE: usize = 1 << 16;

/// `n` bytes from the program at `addr` (copy_from_user after kvmalloc): ENOMEM above kvmalloc's limit or
/// without memory, EFAULT for an address the program does not have.
pub(crate) fn copy_in(ctx: &Ctx, addr: u64, n: u64) -> Result<Vec<u8>, Errno> {
    if n > KVMALLOC_MAX {
        return Err(ENOMEM);
    }
    let n = n as usize;
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| ENOMEM)?;
    let mut piece = [0u8; 256];
    let mut big = Vec::new();
    while v.len() < n {
        let k = (n - v.len()).min(COPY_PIECE);
        let at = addr.checked_add(v.len() as u64).ok_or(EFAULT)?;
        let buf: &mut [u8] = if k <= piece.len() {
            &mut piece[..k]
        } else {
            if big.is_empty() {
                big.try_reserve_exact(COPY_PIECE).map_err(|_| ENOMEM)?;
                big.resize(COPY_PIECE, 0);
            }
            &mut big[..k]
        };
        if !ctx.mem.read(at, buf) {
            return Err(EFAULT);
        }
        v.extend_from_slice(buf);
    }
    Ok(v)
}

/// The `i`-th record of `size` bytes in `b` (zero-extended when the chunk is shorter).
pub(crate) fn rec<T: Wire>(b: &[u8], i: usize) -> T {
    let s = i * T::SIZE;
    let mut tmp = [0u8; 64];
    let have = b.len().saturating_sub(s).min(T::SIZE);
    tmp[..have].copy_from_slice(&b[s..s + have]);
    T::decode(&tmp[..T::SIZE]).unwrap_or(T::ZERO)
}

/// `amdgpu_bo_create_list_entry_array` (amdgpu_bo_list.c) as CachyOS's kernel has it: the entry size must be
/// the struct's (EINVAL before anything is read, also for an empty list; Linux 6.6 took any size).
pub(crate) fn bo_list_entries(ctx: &Ctx, li: &drm_amdgpu_bo_list_in) -> Result<Vec<drm_amdgpu_bo_list_entry>, Errno> {
    let es = drm_amdgpu_bo_list_entry::SIZE as u64;
    if li.bo_info_size as u64 != es {
        return Err(EINVAL);
    }
    let n = li.bo_number as u64;
    let b = copy_in(ctx, li.bo_info_ptr, n * es)?;
    let mut out = Vec::new();
    out.try_reserve_exact(n as usize).map_err(|_| ENOMEM)?;
    out.extend((0..n as usize).map(|i| rec::<drm_amdgpu_bo_list_entry>(&b, i)));
    Ok(out)
}

/// `amdgpu_bo_list_create` (amdgpu_bo_list.c:70): every handle names an object (ENOENT), a userptr object
/// belongs to the caller's address space (EPERM, :116), priorities clamp at 32.
pub(crate) fn bo_list_create(ctx: &Ctx, es: &[drm_amdgpu_bo_list_entry]) -> Result<Vec<(Arc<Bo>, u32)>, Errno> {
    let mut v = Vec::new();
    v.try_reserve_exact(es.len()).map_err(|_| ENOMEM)?;
    for e in es {
        let bo = ctx.file.bo(e.bo_handle).ok_or(ENOENT)?;
        crate::userptr::check_mm(ctx, &bo)?;
        v.push((bo, e.bo_priority.min(BO_MAX_PRIORITY)));
    }
    Ok(v)
}

