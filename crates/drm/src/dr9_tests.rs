//! DR-9 host tests: BO_LIST create/update/destroy and CS by list handle, FENCE_TO_HANDLE (syncobj, syncobj fd,
//! sync_file), AMDGPU_INFO_GPUVM_FAULT after a job's fault, GEM_USERPTR objects over a fake address space (Linux's
//! rules: page-aligned, read-only, ANONONLY, no CPU map or export, EPERM in another address space, invalidation
//! on unmap: the next CS gets EFAULT, a new mapping works), an UNMAP that waits for the VM's queued jobs, and the
//! ioctl fuzz pass over every DRM and amdgpu number (no panic, a Linux errno).

use crate::dr6_tests::{Fx, BASE, ME, P};
use crate::dr8_tests::*;
use crate::fake::{FakeGpu, FakeMemory};
use crate::fence;
use crate::gpu::GpuDevice;
use crate::uapi::*;
use crate::userptr::FakeUserPages;
use crate::wire::Wire;
use amdgpu_fmt::pm4;
use linux::errno::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::vec::Vec;
use std::{thread, vec};

const LIST_AT: usize = 6144;
/// Where the userptr range lives in the fake address space, and where the GPU maps it.
const UADDR: u64 = 0x7f00_0000_0000;
const UVA: u64 = 0x2_0000_0000;
/// Address-space ids: one per test (the registry is the kernel's, shared by tests running at once).
const MM_RULES: u64 = 9_001;
const MM_JOBS: u64 = 9_002;
const MM_FRAMES: u64 = 9_020;
const MM_RO: u64 = 9_003;
const MM_WAIT: u64 = 9_004;
const MM_FUZZ: u64 = 9_005;

fn bo_list(fx: &Fx, op: u32, handle: u32, hs: &[(u32, u32)], size: u32) -> Result<u32, vfs::Errno> {
    let list: Vec<u8> = hs.iter().flat_map(|&(h, p)| bytes(&drm_amdgpu_bo_list_entry { bo_handle: h, bo_priority: p })).collect();
    put_bytes(fx, LIST_AT, &list);
    let mut a = drm_amdgpu_bo_list::ZERO;
    a.set_in(&drm_amdgpu_bo_list_in { operation: op, list_handle: handle, bo_number: hs.len() as u32, bo_info_size: size, bo_info_ptr: BASE + LIST_AT as u64 });
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_BO_LIST)?;
    let o = fx.m.take::<drm_amdgpu_bo_list>(0);
    assert_eq!(o.bytes[4..], [0; 20], "the whole union is zeroed before the answer");
    Ok(o.out().list_handle)
}

fn fence_to_handle(fx: &Fx, ctx_id: u32, seq: u64, what: u32) -> Result<u32, vfs::Errno> {
    let mut a = drm_amdgpu_fence_to_handle::ZERO;
    a.set_in(&drm_amdgpu_fence_to_handle__in { fence: drm_amdgpu_fence { ctx_id, ip_type: 0, ip_instance: 0, ring: 0, seq_no: seq }, what, pad: 0 });
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_FENCE_TO_HANDLE)?;
    let o = fx.m.take::<drm_amdgpu_fence_to_handle>(0);
    assert_eq!(o.bytes[4..], a.bytes[4..], "only out.handle is written");
    Ok(o.out().handle)
}

fn info(fx: &Fx, query: u32, size: u32) -> Result<Vec<u8>, vfs::Errno> {
    let mut req = drm_amdgpu_info::ZERO;
    req.return_pointer = BASE + 4096;
    req.return_size = size;
    req.query = query;
    put_bytes(fx, 4096, &[0xee; 64]);
    fx.m.put(0, &req);
    fx.io(0, DRM_IOCTL_AMDGPU_INFO)?;
    let mut b = vec![0u8; 24];
    assert!(vfs::sys::UserMem::read(&fx.m, BASE + 4096, &mut b));
    Ok(b)
}

fn userptr(fx: &Fx, addr: u64, size: u64, flags: u32) -> Result<u32, vfs::Errno> {
    fx.m.put(0, &drm_amdgpu_gem_userptr { addr, size, flags, handle: 0 });
    fx.io(0, DRM_IOCTL_AMDGPU_GEM_USERPTR)?;
    Ok(fx.m.take::<drm_amdgpu_gem_userptr>(0).handle)
}

/// A fixture whose device has a fake address space `mm` with `pages` writable pages at UADDR.
fn with_space(mm: u64, pages: u64) -> (Fx, Arc<FakeUserPages>) {
    let fx = Fx::new();
    let sp = FakeUserPages::new(mm);
    sp.map(mm, UADDR, pages, true, false);
    fx.gpu.shared().gem.set_user_pages(Some(sp.clone()));
    (fx, sp)
}

/// WRITE_DATA of `vals` to `va`.
fn write_to(va: u64, vals: &[u32]) -> Vec<u32> {
    let mut p = Vec::new();
    pm4::write_data(&mut p, pm4::dst_sel::MEM, va, 0, vals).unwrap();
    p
}

fn lookup_fd(fx: &Fx, fd: u32) -> Arc<dyn vfs::File> {
    fx.fds.get(fd as usize).unwrap().file.clone()
}

#[test]
fn dr9_bo_list_ops_in_linux_order() {
    let fx = Fx::new();
    let a = fx.create(P, GTT, 0).unwrap();
    let b = fx.create(P, GTT, 0).unwrap();
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(a, 3), (b, 99)], 8), Ok(1));
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[], 8), Ok(2), "an empty list is a list");
    let l1 = fx.f(0).drm_file().bo_lists.get(1).unwrap();
    assert_eq!(l1.iter().map(|(_, p)| *p).collect::<Vec<_>>(), [3, 32], "priorities clamp at 32");
    // CachyOS's order: CREATE and UPDATE read the entries (the entry size must be the struct's), DESTROY and an
    // unknown operation read nothing.
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(a, 0)], 12), Err(EINVAL));
    assert_eq!(bo_list(&fx, 7, 1, &[(a, 0)], 12), Err(EINVAL));
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(a, 0), (999, 0)], 8), Err(ENOENT));
    {
        let mut x = drm_amdgpu_bo_list::ZERO;
        x.set_in(&drm_amdgpu_bo_list_in { operation: AMDGPU_BO_LIST_OP_CREATE, list_handle: 0, bo_number: 2, bo_info_size: 8, bo_info_ptr: 8 });
        fx.m.put(0, &x);
        assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_BO_LIST), Err(EFAULT));
        x.set_in(&drm_amdgpu_bo_list_in { operation: AMDGPU_BO_LIST_OP_UPDATE, list_handle: 77, bo_number: 2, bo_info_size: 8, bo_info_ptr: 8 });
        fx.m.put(0, &x);
        assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_BO_LIST), Err(EFAULT), "UPDATE reads the entries before the handle");
        x.set_in(&drm_amdgpu_bo_list_in { operation: 5, list_handle: 1, bo_number: 2, bo_info_size: 8, bo_info_ptr: 8 });
        fx.m.put(0, &x);
        assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_BO_LIST), Err(EINVAL), "an unknown operation reads nothing");
        x.set_in(&drm_amdgpu_bo_list_in { operation: AMDGPU_BO_LIST_OP_CREATE, list_handle: 1, bo_number: 1 << 30, bo_info_size: 8, bo_info_ptr: BASE });
        fx.m.put(0, &x);
        assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_BO_LIST), Err(ENOMEM), "over kvmalloc's limit");
    }
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[], 8), Ok(3));
    {
        let mut x = drm_amdgpu_bo_list::ZERO;
        x.set_in(&drm_amdgpu_bo_list_in { operation: AMDGPU_BO_LIST_OP_DESTROY, list_handle: 3, bo_number: 2, bo_info_size: 12, bo_info_ptr: 8 });
        fx.m.put(0, &x);
        assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_BO_LIST), Ok(0), "DESTROY reads no entries");
    }
    assert_eq!(bo_list(&fx, 3, 0, &[], 8), Err(EINVAL), "unknown operation");
    // UPDATE puts a new list under the handle; an unknown handle is ENOENT.
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_UPDATE, 1, &[(b, 1)], 8), Ok(1));
    assert_eq!(fx.f(0).drm_file().bo_lists.get(1).unwrap().len(), 1);
    assert_eq!(l1.len(), 2, "a holder of the old list keeps it");
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_UPDATE, 9, &[(b, 1)], 8), Err(ENOENT));
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_UPDATE, 0, &[], 8), Err(ENOENT));
    // DESTROY never fails and answers 0; the lowest free handle comes back.
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_DESTROY, 1, &[], 8), Ok(0));
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_DESTROY, 1, &[], 8), Ok(0));
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_DESTROY, 77, &[], 8), Ok(0));
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(a, 0)], 8), Ok(1));
    // Lists are per file.
    assert!(fx.f(1).drm_file().bo_lists.get(1).is_err());
    assert_eq!(fx.f(0).drm_file().bo_lists.len(), 2);
}

#[test]
fn dr9_cs_by_list_handle() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[0xabcd], 9));
    let x = fx.create(P, GTT, 0).unwrap();
    let l = bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(h, 0), (x, 0)], 8).unwrap();
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(1));
    assert_eq!(read(&fx, h, 0x800, 4), 0xabcdu32.to_le_bytes());
    assert!(!fx.bo(0, x).resv().is_empty(), "the listed objects got the job's fence");
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw), bo_handles(&fx, &[h])]), Err(EINVAL), "a handle and a BO_HANDLES chunk");
    assert_eq!(cs_as(&fx, 0, c, l + 1, &[gfx(dw)]), Err(ENOENT));
    // The list holds its objects: a closed handle does not take them from it.
    fx.close(0, x).unwrap();
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(2));
    bo_list(&fx, AMDGPU_BO_LIST_OP_DESTROY, l, &[], 8).unwrap();
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Err(ENOENT));
    assert_eq!(wait_cs(&fx, c, 0, 0, 2, u64::MAX), Ok(0));
}

#[test]
fn dr9_fence_to_handle() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (_h, dw) = setup(&fx, 0, &program(&[1], 1));
    let sin = syncobj(&fx, 0);
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    assert_eq!(cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin])]), Ok(1));
    // A syncobj with the job's fence: not signaled until the job ran.
    let so = fence_to_handle(&fx, c, 1, AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ).unwrap();
    let f = fx.f(0).drm_file().syncobj(so).unwrap().fence().unwrap();
    assert!(!fence::is_signaled(&f));
    let sfd = fence_to_handle(&fx, c, 1, AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ_FD).unwrap();
    let obj = crate::sync_file::as_syncobj(&lookup_fd(&fx, sfd)).expect("a syncobj fd");
    assert!(!fence::is_signaled(&obj.fence().unwrap()));
    let ffd = fence_to_handle(&fx, c, 1, AMDGPU_FENCE_TO_HANDLE_GET_SYNC_FILE_FD).unwrap();
    let sf = crate::sync_file::as_sync_file(&lookup_fd(&fx, ffd)).expect("a sync_file fd");
    assert!(fx.fds.cloexec(ffd as usize).unwrap() && fx.fds.cloexec(sfd as usize).unwrap(), "O_CLOEXEC");
    gate.signal();
    assert!(fence::is_signaled(&f) && fence::is_signaled(sf.fence()) && fence::is_signaled(&obj.fence().unwrap()));
    // seq 0 is known done: a signaled stub.
    let s0 = fence_to_handle(&fx, c, 0, AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ).unwrap();
    assert!(fence::is_signaled(&fx.f(0).drm_file().syncobj(s0).unwrap().fence().unwrap()));
    // The lookup's errors first, then `what`.
    assert_eq!(fence_to_handle(&fx, c + 9, 1, 7), Err(EINVAL), "unknown context");
    assert_eq!(fence_to_handle(&fx, c, 5, AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ), Err(EINVAL), "not handed out");
    assert_eq!(fence_to_handle(&fx, c, 1, 3), Err(EINVAL), "unknown what");
    // No fd table: EMFILE for the fd forms, the handle form works.
    let mut a = drm_amdgpu_fence_to_handle::ZERO;
    a.set_in(&drm_amdgpu_fence_to_handle__in { fence: drm_amdgpu_fence { ctx_id: c, ip_type: 0, ip_instance: 0, ring: 0, seq_no: 1 }, what: AMDGPU_FENCE_TO_HANDLE_GET_SYNC_FILE_FD, pad: 0 });
    fx.m.put(0, &a);
    assert_eq!(fx.f(0).ioctl_with(DRM_IOCTL_AMDGPU_FENCE_TO_HANDLE, BASE, &fx.m, ME, None), Err(EMFILE));
}

#[test]
fn dr9_gpuvm_fault_query() {
    let fx = Fx::new();
    assert_eq!(info(&fx, AMDGPU_INFO_GPUVM_FAULT, 16).unwrap()[..16], [0; 16], "no VM yet: zeros");
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    // A WRITE_DATA to a hole: the job faults and ends with ETIME.
    let (_h, dw) = setup(&fx, 0, &write_to(0x5_0000_3010, &[1]));
    assert_eq!(cs(&fx, c, &[gfx(dw)]), Ok(1));
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, u64::MAX), Err(ETIME));
    let b = info(&fx, AMDGPU_INFO_GPUVM_FAULT, 16).unwrap();
    let addr = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let status = u32::from_le_bytes(b[8..12].try_into().unwrap());
    assert_eq!(addr, 0x5_0000_3000, "the page");
    assert_eq!(status & (1 << 8), 1 << 8, "MAPPING_ERROR");
    assert_eq!((status >> 9) & 0x1ff, 6, "client CPG");
    assert_eq!((status >> 18) & 1, 1, "a write");
    assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), AMDGPU_VMHUB_TYPE_GFX);
    // A short buffer gets the first bytes only.
    let b = info(&fx, AMDGPU_INFO_GPUVM_FAULT, 8).unwrap();
    assert_eq!(&b[8..12], &[0xee; 4]);
    // The other file's VM saw nothing.
    let mut req = drm_amdgpu_info::ZERO;
    req.return_pointer = BASE + 4096;
    req.return_size = 16;
    req.query = AMDGPU_INFO_GPUVM_FAULT;
    fx.m.put(0, &req);
    fx.io(1, DRM_IOCTL_AMDGPU_INFO).unwrap();
    assert_eq!(fx.m.take::<[u8; 16]>(4096), [0; 16]);
}

/// CR-GX7m2-2: an IB in a page mapped without AMDGPU_VM_PAGE_EXECUTABLE faults as on the 780M (29.10.:
/// PERMISSION_FAULTS 0x8, a read by CPF) and writes nothing; the same IB in an executable page runs.
#[test]
fn dr9_ib_fetch_needs_the_execute_bit() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let h = fx.create_in(0, 8192, 0, GTT, 0).unwrap();
    map(&fx, 0, h, V, 8192, AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE);
    let cmds: Vec<u8> = write_to(V + 0x800, &[0x5a]).iter().flat_map(|d| d.to_le_bytes()).collect();
    assert!(fx.f(0).drm_file().bo(h).unwrap().write(0, &cmds));
    assert_eq!(cs(&fx, c, &[gfx((cmds.len() / 4) as u32)]), Ok(1));
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, u64::MAX), Err(ETIME), "the fetch faults");
    assert_eq!(read(&fx, h, 0x800, 4), [0; 4], "nothing ran");
    let st = info(&fx, AMDGPU_INFO_GPUVM_FAULT, 16).unwrap();
    assert_eq!(u64::from_le_bytes(st[0..8].try_into().unwrap()), V, "the IB's page");
    let status = u32::from_le_bytes(st[8..12].try_into().unwrap());
    assert_eq!((status >> 4) & 0xf, 0x8, "PERMISSION_FAULTS: the execute bit alone");
    assert_eq!(status & (1 << 8 | 1 << 18), 0, "not a hole, a read");
    assert_eq!((status >> 9) & 0x1ff, 6, "client CPG");
    // Executable now: the same IB runs.
    let c2 = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let h2 = fx.create_in(0, 8192, 0, GTT, 0).unwrap();
    map(&fx, 0, h2, V + 0x10_0000, 8192, RW);
    let cmds: Vec<u8> = write_to(V + 0x10_0800, &[0x5a]).iter().flat_map(|d| d.to_le_bytes()).collect();
    assert!(fx.f(0).drm_file().bo(h2).unwrap().write(0, &cmds));
    assert_eq!(cs(&fx, c2, &[ib(0, 0, V + 0x10_0000, (cmds.len() / 4) as u32, 0)]), Ok(1));
    assert_eq!(wait_cs(&fx, c2, 0, 0, 1, u64::MAX), Ok(0));
    assert_eq!(read(&fx, h2, 0x800, 4), 0x5au32.to_le_bytes());
}

#[test]
fn dr9_userptr_create_rules() {
    let fx = Fx::new();
    // Without a source of user pages: ENODEV after the argument checks (DR-6's behaviour).
    assert_eq!(userptr(&fx, UADDR, P, AMDGPU_GEM_USERPTR_READONLY), Err(ENODEV));
    let (fx, sp) = with_space(MM_RULES, 4);
    assert_eq!(userptr(&fx, UADDR + 1, P, AMDGPU_GEM_USERPTR_READONLY), Err(EINVAL));
    assert_eq!(userptr(&fx, UADDR, P + 1, AMDGPU_GEM_USERPTR_READONLY), Err(EINVAL));
    assert_eq!(userptr(&fx, UADDR, P, 1 << 4), Err(EINVAL));
    assert_eq!(userptr(&fx, UADDR, P, 0), Err(EACCES), "writable needs REGISTER");
    assert_eq!(userptr(&fx, UADDR, 0, AMDGPU_GEM_USERPTR_READONLY), Err(ENOSPC));
    assert_eq!(userptr(&fx, 0x10000, 1 << 41, AMDGPU_GEM_USERPTR_READONLY), Err(ENOSPC), "past the offset range");
    assert_eq!(userptr(&fx, 0x10000, 1 << 39, AMDGPU_GEM_USERPTR_READONLY), Err(ENOMEM), "the page array");
    // CachyOS 7.3's access_ok: after the alignment, before the flags.
    const K: u64 = 0xffff_8880_0000_0000;
    assert_eq!(userptr(&fx, K, P, AMDGPU_GEM_USERPTR_READONLY), Err(EFAULT));
    assert_eq!(userptr(&fx, K, P, 0), Err(EFAULT), "before EACCES");
    assert_eq!(userptr(&fx, K, P, 0x11), Err(EFAULT), "before the flag check");
    assert_eq!(userptr(&fx, K + 1, P, 0), Err(EINVAL), "after the alignment");
    assert_eq!(userptr(&fx, K, 0, AMDGPU_GEM_USERPTR_READONLY), Err(EFAULT));
    assert_eq!(userptr(&fx, 0x7fff_ffff_f000, 0xffff_8000_0000_1000, AMDGPU_GEM_USERPTR_READONLY), Err(EFAULT), "wraps");
    assert_eq!(userptr(&fx, 0x7fff_ffff_e000, 2 * P, AMDGPU_GEM_USERPTR_READONLY), Err(EFAULT), "past the top");
    assert_eq!(userptr(&fx, UADDR, 1 << 41, AMDGPU_GEM_USERPTR_READONLY), Err(EFAULT), "UADDR + 2 TiB is past the top");
    // Without VALIDATE the pages are not looked at: an unmapped range is an object too.
    let lazy = userptr(&fx, 0x1000_0000, P, AMDGPU_GEM_USERPTR_REGISTER).unwrap();
    assert_eq!(userptr(&fx, 0x1000_0000, P, AMDGPU_GEM_USERPTR_REGISTER | AMDGPU_GEM_USERPTR_VALIDATE), Err(EFAULT));
    assert_eq!(userptr(&fx, UADDR, 5 * P, AMDGPU_GEM_USERPTR_REGISTER | AMDGPU_GEM_USERPTR_VALIDATE), Err(EFAULT), "a hole after 4 pages");
    sp.map(MM_RULES, 0x2000_0000, 1, true, true);
    assert_eq!(userptr(&fx, 0x2000_0000, P, AMDGPU_GEM_USERPTR_READONLY | AMDGPU_GEM_USERPTR_ANONONLY | AMDGPU_GEM_USERPTR_VALIDATE), Err(EPERM));
    sp.map(MM_RULES, 0x3000_0000, 1, false, false);
    assert_eq!(userptr(&fx, 0x3000_0000, P, AMDGPU_GEM_USERPTR_REGISTER | AMDGPU_GEM_USERPTR_VALIDATE), Err(EFAULT), "read-only page, writable object");
    let ro = userptr(&fx, 0x3000_0000, P, AMDGPU_GEM_USERPTR_READONLY | AMDGPU_GEM_USERPTR_VALIDATE).unwrap();
    let h = userptr(&fx, UADDR, 4 * P, AMDGPU_GEM_USERPTR_REGISTER | AMDGPU_GEM_USERPTR_VALIDATE | AMDGPU_GEM_USERPTR_ANONONLY).unwrap();
    assert_eq!(crate::userptr::registered_in(MM_RULES), 3);
    // Linux's object rules: GTT, no CPU map, no export, no placement change.
    let i = fx.info(0, h).unwrap();
    assert_eq!((i.bo_size, i.domains), (4 * P, AMDGPU_GEM_DOMAIN_GTT as u64));
    put_bytes(&fx, 0, &[0; 8]);
    put_bytes(&fx, 0, &h.to_le_bytes());
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_GEM_MMAP), Err(EPERM));
    assert_eq!(fx.to_fd(0, h, 0), Err(EPERM));
    assert_eq!(fx.op(h, AMDGPU_GEM_OP_SET_PLACEMENT, GTT), Err(EPERM));
    // A user fence object must not be one.
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (_ib, dw) = setup(&fx, 0, &program(&[1], 1));
    assert_eq!(cs(&fx, c, &[gfx(dw), uf(ro, 0)]), Err(EINVAL));
    for x in [lazy, ro, h] {
        fx.close(0, x).unwrap();
    }
    assert_eq!(crate::userptr::registered_in(MM_RULES), 0, "closed objects leave the registry");
}

/// The frames a device's page table must hold: the pages of now, which a revalidation replaces.
#[test]
fn dr9_userptr_frames_are_the_pages_of_now() {
    use crate::dma_buf::Exporter;
    let (fx, sp) = with_space(MM_FRAMES, 2);
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let u = userptr(&fx, UADDR, 2 * P, AMDGPU_GEM_USERPTR_REGISTER).unwrap();
    let bo = fx.bo(0, u);
    let mem = bo.userptr().unwrap();
    assert!(mem.frames().is_none(), "no pages before the first validation");
    let (h, dw) = setup(&fx, 0, &write_to(UVA, &[1]));
    map(&fx, 0, u, UVA, 2 * P, RW);
    let l = bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(h, 0), (u, 0)], 8).unwrap();
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(1));
    let first = mem.frames().expect("pages after a CS");
    assert_eq!(first.size(), 2 * P);
    // Unmapped and mapped anew: the next CS gets other pages, and the old object stays with whoever holds it.
    assert_eq!(sp.unmap(MM_FRAMES, UADDR, 2), 1);
    sp.map(MM_FRAMES, UADDR, 2, true, false);
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(2));
    let second = mem.frames().expect("pages after the revalidation");
    assert!(!Arc::ptr_eq(&first, &second), "a revalidation that kept the old pages");
}

#[test]
fn dr9_userptr_jobs_and_invalidation() {
    let (fx, sp) = with_space(MM_JOBS, 2);
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    assert!(sp.write(MM_JOBS, UADDR + 0x10, &0x5555_aaaau32.to_le_bytes()));
    let u = userptr(&fx, UADDR, 2 * P, AMDGPU_GEM_USERPTR_REGISTER).unwrap();
    map(&fx, 0, u, UVA, 2 * P, RW);
    // The job writes the program's own memory through the GPU VM.
    let (h, dw) = setup(&fx, 0, &write_to(UVA + P + 8, &[0x1234_5678, 0x9abc_def0]));
    let l = bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(h, 0), (u, 0)], 8).unwrap();
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(1));
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, u64::MAX), Ok(0));
    let mut b = [0u8; 8];
    assert!(sp.read(MM_JOBS, UADDR + P + 8, &mut b));
    assert_eq!(b, [0x78, 0x56, 0x34, 0x12, 0xf0, 0xde, 0xbc, 0x9a], "the program sees the GPU's write");
    // The program unmaps the range: the object is told, the next CS that lists it gets EFAULT.
    assert_eq!(sp.unmap(MM_JOBS, UADDR, 2), 1);
    assert!(!fx.bo(0, u).userptr().unwrap().current());
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Err(EFAULT));
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Err(EFAULT), "until it is mapped again");
    // A new mapping at the address: new pages, the job writes them.
    sp.map(MM_JOBS, UADDR, 2, true, false);
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(2));
    assert_eq!(wait_cs(&fx, c, 0, 0, 2, u64::MAX), Ok(0));
    assert!(sp.read(MM_JOBS, UADDR + P + 8, &mut b));
    assert_eq!(b[..4], [0x78, 0x56, 0x34, 0x12]);
    // A forked child with the parent's fd: EPERM for its lists and CS (another address space).
    sp.set_current(MM_JOBS + 1);
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(u, 0)], 8), Err(EPERM));
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Err(EPERM));
    assert_eq!(cs_as(&fx, 0, c, 0, &[gfx(dw), bo_handles(&fx, &[u])]), Err(EPERM));
    sp.set_current(MM_JOBS);
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(3), "back in the creator's address space");
}

#[test]
fn dr9_userptr_readonly_takes_no_gpu_write() {
    let (fx, sp) = with_space(MM_RO, 1);
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let u = userptr(&fx, UADDR, P, AMDGPU_GEM_USERPTR_READONLY).unwrap();
    map(&fx, 0, u, UVA, P, RW);
    let (h, dw) = setup(&fx, 0, &write_to(UVA, &[7]));
    assert_eq!(cs(&fx, c, &[gfx(dw), bo_handles(&fx, &[h, u])]), Ok(1));
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, u64::MAX), Err(ETIME), "the write faults");
    let mut b = [9u8; 4];
    assert!(sp.read(MM_RO, UADDR, &mut b));
    assert_eq!(b, [0; 4], "the program's memory is untouched");
    let st = info(&fx, AMDGPU_INFO_GPUVM_FAULT, 16).unwrap();
    let status = u32::from_le_bytes(st[8..12].try_into().unwrap());
    assert_eq!((status >> 4) & 0xf, 0x4, "a write permission fault");
    assert_eq!(status & (1 << 8), 0, "not a hole");
}

/// An unmap waits for the GPU jobs on the object (bounded), as Linux's notifier does.
#[test]
fn dr9_userptr_unmap_waits_for_the_job() {
    let (fx, sp) = with_space(MM_WAIT, 1);
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let u = userptr(&fx, UADDR, P, AMDGPU_GEM_USERPTR_REGISTER).unwrap();
    map(&fx, 0, u, UVA, P, RW);
    let (h, dw) = setup(&fx, 0, &write_to(UVA, &[0x77]));
    let sin = syncobj(&fx, 0);
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    assert_eq!(cs(&fx, c, &[gfx(dw), bo_handles(&fx, &[h, u]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin])]), Ok(1));
    let t = Instant::now();
    let sp2 = sp.clone();
    let j = thread::spawn(move || sp2.unmap(MM_WAIT, UADDR, 1));
    thread::sleep(Duration::from_millis(100));
    assert!(!j.is_finished(), "the unmap waits for the job");
    gate.signal();
    assert_eq!(j.join().unwrap(), 1);
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, u64::MAX), Ok(0), "the job ran on the pages it was submitted with");
}

/// Finding 8 of DR-8, the other half: after the deferred Clear the next job no longer sees the mapping (it
/// waits for the Clear through the VM's last update), and a dropped VM ends its deferred updates.
#[test]
fn dr9_cs_after_unmap_sees_the_clear() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[3], 3));
    let x = fx.create_in(0, 2 * P, 0, GTT, 0).unwrap();
    map(&fx, 0, x, 0x3_0000_0000, 2 * P, RW);
    let sin = syncobj(&fx, 0);
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    assert_eq!(cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin])]), Ok(1));
    let a = drm_amdgpu_gem_va { handle: x, operation: AMDGPU_VA_OP_UNMAP, flags: 0, va_address: 0x3_0000_0000, offset_in_bo: 0, map_size: 2 * P, ..drm_amdgpu_gem_va::ZERO };
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_GEM_VA).unwrap();
    let vm = fx.f(0).drm_file().vm.backend_vm().unwrap();
    assert_eq!(vm.as_fake().unwrap().deferred(), 1, "the Clear waits for job 1");
    assert!(vm.as_fake().unwrap().translate(0x3_0000_0000).is_some());
    // Job 2 writes where x was: it runs after the Clear and faults.
    let (h2, dw2) = {
        let p = write_to(0x3_0000_1000, &[5]);
        let b: Vec<u8> = p.iter().flat_map(|d| d.to_le_bytes()).collect();
        assert!(fx.bo(0, h).write(0x400, &b));
        (h, p.len() as u32)
    };
    let _ = h2;
    assert_eq!(cs(&fx, c, &[ib(AMDGPU_HW_IP_GFX, 0, V + 0x400, dw2, 0)]), Ok(2));
    gate.signal();
    assert_eq!(read(&fx, h, 0x800, 4), 3u32.to_le_bytes(), "job 1 ran with its mapping");
    // Job 2 faulted; WAIT_CS answers the entity's error for every number then (amdgpu_ctx_get_entity).
    assert_eq!(wait_cs(&fx, c, 0, 0, 2, u64::MAX), Err(ETIME), "job 2 saw the range cleared");
    assert_eq!(vm.last_fault().map(|f| f.addr), Some(0x3_0000_1000));
    assert_eq!(vm.as_fake().unwrap().deferred(), 0);
    assert!(vm.as_fake().unwrap().translate(0x3_0000_0000).is_none());
}

#[test]
fn dr9_closed_file_ends_deferred_updates() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[3], 3));
    let sin = syncobj(&fx, 0);
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    assert_eq!(cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin])]), Ok(1));
    let a = drm_amdgpu_gem_va { handle: h, operation: AMDGPU_VA_OP_CLEAR, flags: 0, va_address: V, offset_in_bo: 0, map_size: 2 * P, ..drm_amdgpu_gem_va::ZERO };
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_GEM_VA).unwrap();
    let last = fx.f(0).drm_file().vm.last_update().expect("the deferred Clear's fence");
    assert!(!fence::is_signaled(&last));
    let mut fx = fx;
    fx.files[0] = None;
    gate.signal();
    assert!(fence::is_signaled(&last), "no waiter hangs on a dropped VM");
}

// ---- the fuzz pass

/// xorshift64*: the same numbers on every run, so a failure can be found again by its iteration.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Every errno Linux 6.6 has (include/uapi/asm-generic/errno-base.h, errno.h: 1..=133).
fn linux_errno(e: vfs::Errno) -> bool {
    (1..=133).contains(&e)
}

/// One fuzz call: a random command for number `nr` (direction, size), a random argument pointer (the test
/// memory, its last bytes, NULL, a kernel address, anything) over random memory.
fn fuzz_one(fx: &Fx, r: &mut Rng, nr: u32, sizes: &[u32]) -> (u32, Result<i64, vfs::Errno>) {
    let size = match r.below(4) {
        0 => sizes[r.below(sizes.len() as u64) as usize],
        1 => r.below(0x80) as u32,
        2 => r.below(0x4000) as u32,
        _ => 0,
    };
    let dir = r.below(4) as u32;
    let cmd = (dir << 30) | (size << 16) | (0x64 << 8) | nr;
    let arg = match r.below(6) {
        0 => 0,
        1 => BASE + 8192 - r.below(64),
        2 => 0xffff_8000_0000_0000 + r.below(1 << 20),
        3 => r.next(),
        _ => BASE + r.below(64) * 8,
    };
    // Random memory, with small numbers and pointers into the test memory among it.
    let mut b = vec![0u8; 8192];
    for w in b.chunks_mut(8) {
        let v = match r.below(5) {
            0 => r.next(),
            1 => r.below(8),
            2 => BASE + r.below(8192),
            3 => u64::MAX,
            _ => 0,
        };
        w.copy_from_slice(&v.to_le_bytes());
    }
    put_bytes(fx, 0, &b);
    let k = r.below(2) as usize;
    (cmd, fx.f(k).ioctl_with(cmd, arg, &fx.m, ME, Some(fx.fds.clone())))
}

/// Every DRM core and amdgpu number with random sizes, directions, pointers and contents: no panic, every
/// answer Ok or a Linux errno; a number without a table entry is EINVAL, another ioctl type ENOTTY. Waits in
/// it end at once (the test memory says "killed": EINTR), the device's memory is small so no create can take
/// the host's.
#[test]
fn dr9_fuzz_every_ioctl_number() {
    let small = FakeMemory { carveout: 64 << 20, visible: 64 << 20, gtt: 64 << 20, ..FakeMemory::CAPTURED };
    let fx = Fx::with(FakeGpu::radeon_780m_with(small));
    fx.m.kill.store(true, std::sync::atomic::Ordering::SeqCst);
    let sp = FakeUserPages::new(MM_FUZZ);
    sp.map(MM_FUZZ, BASE, 2, true, false);
    fx.gpu.shared().gem.set_user_pages(Some(sp.clone()));
    // Live objects for the small numbers the fuzz writes: objects 1 and 2 (one mapped), a context, a syncobj,
    // a BO list in file 0.
    fx.m.kill.store(false, std::sync::atomic::Ordering::SeqCst);
    let (h, _) = setup(&fx, 0, &program(&[1], 1));
    let h2 = fx.create(P, GTT, 0).unwrap();
    ctx_alloc(&fx, 0, 0, ME).unwrap();
    syncobj(&fx, 0);
    bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(h, 0), (h2, 0)], 8).unwrap();
    fx.m.kill.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut r = Rng(0x9e37_79b9_7f4a_7c15);
    // Real sizes first: those of the numbers' table entries (and GEM_VA's 64-byte libdrm form).
    let mut sizes: Vec<u32> = vec![8, 16, 24, 32, 40, 64];
    for (_, cmd) in crate::uapi::AMDGPU_IOCTLS.iter().chain(crate::uapi::DRM_IOCTLS) {
        sizes.push(crate::ioc::size(*cmd));
    }
    let t = Instant::now();
    let mut n = 0u64;
    for round in 0..400 {
        for nr in 0u32..0x100 {
            let (cmd, res) = fuzz_one(&fx, &mut r, nr, &sizes);
            n += 1;
            if let Err(e) = res {
                assert!(linux_errno(e), "round {round} cmd {cmd:#x}: errno {e}");
            }
        }
    }
    // Another type is never DRM's.
    assert_eq!(fx.io(0, 0xc010_6540), Err(ENOTTY));
    // Numbers without an entry: EINVAL.
    let mut holes = 0;
    for nr in 0u32..0x100 {
        let known = if (DRM_COMMAND_BASE..DRM_COMMAND_END).contains(&nr) {
            crate::fake::AMDGPU_TABLE.get((nr - DRM_COMMAND_BASE) as usize).copied().flatten().is_some()
        } else {
            crate::core_ioctls::TABLE.get(nr as usize).copied().flatten().is_some()
        };
        if !known {
            holes += 1;
            assert_eq!(fx.io(0, (3 << 30) | (8 << 16) | (0x64 << 8) | nr), Err(EINVAL), "nr {nr:#x}");
        }
    }
    assert!(holes > 0);
    // The wait numbers once more without the kill: their real timeout paths, with absolute timeouts in the
    // past or a few milliseconds ahead (amdgpu_gem_timeout, drm_timeout_abs_to_jiffies).
    fx.m.kill.store(false, std::sync::atomic::Ordering::SeqCst);
    let waits: [(u32, usize); 5] = [
        (DRM_IOCTL_AMDGPU_WAIT_CS, 8),
        (DRM_IOCTL_AMDGPU_GEM_WAIT_IDLE, 8),
        (DRM_IOCTL_AMDGPU_WAIT_FENCES, 16),
        (DRM_IOCTL_SYNCOBJ_WAIT, 8),
        (DRM_IOCTL_SYNCOBJ_TIMELINE_WAIT, 16),
    ];
    let tw = Instant::now();
    for round in 0..200 {
        for &(cmd, at) in &waits {
            let mut b = vec![0u8; 8192];
            for w in b.chunks_mut(8) {
                let v = match r.below(4) {
                    0 => r.next(),
                    1 => r.below(8),
                    2 => BASE + r.below(8192),
                    _ => 0,
                };
                w.copy_from_slice(&v.to_le_bytes());
            }
            let now = fence::now().unwrap();
            let timeout = match r.below(3) {
                0 => 0,
                1 => now.saturating_sub(r.below(1 << 20)),
                _ => now + r.below(3_000_000),
            };
            b[at..at + 8].copy_from_slice(&timeout.to_le_bytes());
            put_bytes(&fx, 0, &b);
            let k = r.below(2) as usize;
            if let Err(e) = fx.f(k).ioctl_with(cmd, BASE, &fx.m, ME, Some(fx.fds.clone())) {
                assert!(linux_errno(e) && e != EINTR, "wait round {round} cmd {cmd:#x}: errno {e}");
            }
        }
    }
    assert!(tw.elapsed() < Duration::from_secs(30), "bounded waits took {:?}", tw.elapsed());
    // The device still works after the fuzz: a fresh file builds a list and runs a job on it, the fuzzed file
    // still gets objects.
    let mut fx = fx;
    fx.reopen(0);
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[0x600d], 1));
    let l = bo_list(&fx, AMDGPU_BO_LIST_OP_CREATE, 0, &[(h, 0)], 8).unwrap();
    assert_eq!(cs_as(&fx, 0, c, l, &[gfx(dw)]), Ok(1));
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, u64::MAX), Ok(0));
    assert_eq!(read(&fx, h, 0x800, 4), 0x600du32.to_le_bytes());
    assert_eq!(bo_list(&fx, AMDGPU_BO_LIST_OP_DESTROY, l, &[], 8), Ok(0));
    assert_eq!(fx.close(0, h), Ok(0));
    let h1 = fx.create_in(1, P, 0, GTT, 0).unwrap();
    assert_eq!(fx.close(1, h1), Ok(0));
    assert!(t.elapsed() < Duration::from_secs(120), "{n} calls took {:?}", t.elapsed());
}

#[test]
fn dr9_fake_vm_drop_signals_deferred() {
    use crate::gpu::{GpuVm, PtOp};
    let vm = crate::fake_vm::FakeVm::new(1 << 20).unwrap();
    let gate = crate::fence::Fence::new(crate::fence::context_alloc(1), 1, "test", "gate");
    let r = vm.update_after(&[PtOp::Clear { va: 0x1000, pages: 1 }], core::slice::from_ref(&gate));
    assert_eq!((r.done, r.error), (1, None));
    let f = r.fence.unwrap();
    assert_eq!(vm.deferred(), 1);
    // A later update without dependencies still queues behind it (order).
    let r2 = vm.update_after(&[PtOp::Clear { va: 0x2000, pages: 1 }], &[]);
    assert!(r2.fence.is_some() && vm.deferred() == 2);
    drop(vm);
    assert!(fence::is_signaled(&f) && fence::is_signaled(r2.fence.as_ref().unwrap()));
    assert_eq!(gate.callbacks(), 0, "the registration went with the VM");
}

/// Checker finding: a MAP (through `update` or `update_after` without dependencies) after a deferred UNMAP of
/// the same range never runs first, so the range ends mapped.
#[test]
fn dr9_fake_vm_map_after_deferred_clear_keeps_order() {
    use crate::gpu::{GpuVm, PtOp};
    let vm = crate::fake_vm::FakeVm::new(1 << 20).unwrap();
    let fx = Fx::new();
    let x = fx.create(P, GTT, 0).unwrap();
    let mem = fx.bo(0, x).memory().clone();
    let m = |va| PtOp::Map { va, pages: 1, mem: mem.clone(), first: 0, place: None, uswc: false, flags: 0 };
    assert_eq!(vm.update(&[m(0x1000)]).error, None);
    assert!(vm.translate(0x1000).is_some());
    let gate = crate::fence::Fence::new(crate::fence::context_alloc(1), 1, "test", "gate");
    let c = vm.update_after(&[PtOp::Clear { va: 0x1000, pages: 1 }], core::slice::from_ref(&gate));
    let r1 = vm.update(&[m(0x1000)]);
    let r2 = vm.update_after(&[m(0x2000)], &[]);
    assert_eq!(vm.deferred(), 3, "both maps wait behind the Clear");
    assert!(r1.fence.is_some() && r2.fence.is_some());
    assert!(vm.translate(0x2000).is_none(), "nothing overtook the Clear");
    gate.signal();
    assert!(fence::is_signaled(c.fence.as_ref().unwrap()) && fence::is_signaled(r1.fence.as_ref().unwrap()));
    assert_eq!(vm.deferred(), 0);
    assert!(vm.translate(0x1000).is_some(), "the MAP ran after the Clear, not before");
    assert!(vm.translate(0x2000).is_some());
}

/// Checker finding: an empty or inverted range from MM touches nothing and never panics.
#[test]
fn dr9_userptr_invalidate_odd_ranges() {
    const MM_ODD: u64 = 9_006;
    let (fx, _sp) = with_space(MM_ODD, 2);
    let u = userptr(&fx, UADDR, 2 * P, AMDGPU_GEM_USERPTR_REGISTER | AMDGPU_GEM_USERPTR_VALIDATE).unwrap();
    assert_eq!(crate::userptr::invalidate(MM_ODD, u64::MAX, 0), 0);
    assert_eq!(crate::userptr::invalidate(MM_ODD, UADDR + 4 * P, UADDR + P), 0);
    assert_eq!(crate::userptr::invalidate(MM_ODD, UADDR + P, UADDR + P), 0);
    assert!(fx.bo(0, u).userptr().unwrap().current(), "no odd range invalidated it");
    assert_eq!(crate::userptr::invalidate(MM_ODD, UADDR + P, UADDR + P + 1), 1);
    assert!(!fx.bo(0, u).userptr().unwrap().current());
}
