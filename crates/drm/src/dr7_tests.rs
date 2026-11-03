//! DR-7 host tests: GEM_VA's checks in Linux's order, MAP/UNMAP/CLEAR/REPLACE on the per-file interval map and
//! on the fake's tables, PRT, DELAY_UPDATE, the bo_va going with the last handle of a file, the 40- and 64-byte
//! forms, the timeline out-syncobj, AMDGPU_VM's VMID reservation, fdinfo's memory lines, and nothing left behind
//! at the last close. Expected values are Linux 6.6's, with the VA bounds the device reports (the captured kernel's
//! DEV_INFO: 64 KiB at the bottom).

use crate::dr6_tests::{Fx, BASE, ME, P};
use crate::fake_vm::{Held, TableWalk};
use crate::gpu::GpuDevice;
use crate::ioc;
use crate::syncobj::Syncobj;
use crate::uapi::*;
use crate::vm::{Limits, MapInfo};
use crate::wire::Wire;
use linux::errno::*;
use std::sync::Arc;
use std::thread;
use std::vec::Vec;
use vfs::File;

const GTT: u64 = AMDGPU_GEM_DOMAIN_GTT as u64;
const VRAM: u64 = AMDGPU_GEM_DOMAIN_VRAM as u64;
const RW: u32 = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE;
const V: u64 = 0x1_0000_0000;
/// Linux 6.6's 40-byte GEM_VA command.
const VA40: u32 = ioc::ioc(ioc::IOC_WRITE, DRM_IOCTL_BASE, 0x48, 40);

#[allow(clippy::too_many_arguments)]
fn va(fx: &Fx, k: usize, handle: u32, op: u32, flags: u32, addr: u64, offset: u64, size: u64) -> Result<i64, vfs::Errno> {
    let a = drm_amdgpu_gem_va { handle, operation: op, flags, va_address: addr, offset_in_bo: offset, map_size: size, ..drm_amdgpu_gem_va::ZERO };
    fx.m.put(0, &a);
    fx.io(k, DRM_IOCTL_AMDGPU_GEM_VA)
}

fn map(fx: &Fx, h: u32, addr: u64, offset: u64, size: u64) -> Result<i64, vfs::Errno> {
    va(fx, 0, h, AMDGPU_VA_OP_MAP, RW, addr, offset, size)
}

fn vm_op(fx: &Fx, k: usize, op: u32, flags: u32) -> Result<i64, vfs::Errno> {
    fx.m.put(0, &drm_amdgpu_vm_in { op, flags });
    fx.io(k, DRM_IOCTL_AMDGPU_VM)
}

fn maps(fx: &Fx, k: usize) -> Vec<MapInfo> {
    fx.f(k).drm_file().vm.mappings()
}

/// The fake VM's translation of `addr`: (memory page or None for PRT, flags).
fn tr(fx: &Fx, k: usize, addr: u64) -> Option<(Option<u64>, u32)> {
    let vm = fx.f(k).drm_file().vm.backend_vm()?;
    let t = vm.as_fake().unwrap().translate(addr)?;
    Some((matches!(t.held, Held::Mem { .. }).then_some(t.page), t.flags))
}

fn pieces(fx: &Fx, k: usize) -> usize {
    fx.f(k).drm_file().vm.backend_vm().map_or(0, |v| v.as_fake().unwrap().pieces())
}

fn info(o: u64, size: u64, va: u64, flags: u32) -> MapInfo {
    MapInfo { va, size, offset: o, flags, prt: false }
}

#[test]
fn limits_follow_dev_info() {
    let fx = Fx::new();
    let l = Limits::of(&fx.gpu.dev_info());
    assert_eq!(l, Limits { bottom: 0x1_0000, top: 0xffff_ffbf_0000 });
}

#[test]
fn checks_in_linux_order() {
    let fx = Fx::new();
    let h = fx.create(4 * P, GTT, 0).unwrap();
    // amdgpu_gem.c:685: below the reserved bottom; :692: in the hole; :704: into the top reserved area.
    assert_eq!(map(&fx, h, 0xf000, 0, P), Err(EINVAL));
    assert_eq!(map(&fx, h, 0x0000_8000_0000_0000, 0, P), Err(EINVAL));
    assert_eq!(map(&fx, h, 0xffff_7fff_ffff_f000, 0, P), Err(EINVAL));
    assert_eq!(map(&fx, h, 0xffff_ffff_ffbf_0000, 0, P), Err(EINVAL));
    assert_eq!(map(&fx, h, 0xffff_ffff_ffbe_f000, 0, 2 * P), Err(EINVAL));
    assert_eq!(map(&fx, h, V, 0, u64::MAX - P), Err(EINVAL), "an overflowing end");
    // The range check comes before the handle: an unknown handle in a bad range is EINVAL.
    assert_eq!(map(&fx, 0x7777, 0x1000, 0, P), Err(EINVAL));
    // :719: invalid flag combinations (PRT only with DELAY_UPDATE).
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_MAP, 0x400, V, 0, P), Err(EINVAL));
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_MAP, AMDGPU_VM_PAGE_PRT | AMDGPU_VM_PAGE_READABLE, V, 0, P), Err(EINVAL));
    // :725: operations.
    for op in [0, 5, 0xffff_ffff] {
        assert_eq!(va(&fx, 0, h, op, RW, V, 0, P), Err(EINVAL));
    }
    // :734: the handle.
    assert_eq!(map(&fx, 0x7777, V, 0, P), Err(ENOENT));
    assert_eq!(va(&fx, 0, 0x7777, AMDGPU_VA_OP_UNMAP, 0, V, 0, P), Err(ENOENT));
    // amdgpu_vm_bo_map: alignment, zero size, beyond the object.
    assert_eq!(map(&fx, h, V + 8, 0, P), Err(EINVAL));
    assert_eq!(map(&fx, h, V, 8, P), Err(EINVAL));
    assert_eq!(map(&fx, h, V, 0, P + 8), Err(EINVAL));
    assert_eq!(map(&fx, h, V, 0, 0), Err(EINVAL));
    assert_eq!(map(&fx, h, V, P, 4 * P), Err(EINVAL));
    assert_eq!(map(&fx, h, V, !0xfff, P), Err(EINVAL));
    assert!(maps(&fx, 0).is_empty(), "nothing refused left a mapping");
    // The ends of the user range work: the captured bottom and the last page below the top.
    assert_eq!(map(&fx, h, 0x1_0000, 0, P), Ok(0));
    assert_eq!(map(&fx, h, 0xffff_ffff_ffbe_f000, 0, P), Ok(0));
    // CLEAR and PRT need no handle.
    assert_eq!(va(&fx, 0, 0x7777, AMDGPU_VA_OP_CLEAR, 0, V, 0, P), Ok(0));
    assert_eq!(va(&fx, 0, 0x7777, AMDGPU_VA_OP_MAP, AMDGPU_VM_PAGE_PRT, V, 0, P), Ok(0));
}

#[test]
fn map_unmap_and_translate() {
    let fx = Fx::new();
    let h = fx.create(4 * P, GTT, 0).unwrap();
    assert_eq!(map(&fx, h, V, 0, 4 * P), Ok(0));
    assert_eq!(maps(&fx, 0), [info(0, 4 * P, V, RW)]);
    assert_eq!(tr(&fx, 0, V), Some((Some(0), RW)));
    assert_eq!(tr(&fx, 0, V + 3 * P + 5), Some((Some(3), RW)));
    assert_eq!(tr(&fx, 0, V + 4 * P), None);
    // Overlaps are EINVAL (amdgpu_vm.c:1546), also by one page at either end.
    assert_eq!(map(&fx, h, V + 3 * P, 0, P), Err(EINVAL));
    assert_eq!(map(&fx, h, V - P, 0, 2 * P), Err(EINVAL));
    // The same object twice, a part of it, and at a canonical high address.
    assert_eq!(map(&fx, h, V + 8 * P, 2 * P, 2 * P), Ok(0));
    assert_eq!(tr(&fx, 0, V + 9 * P), Some((Some(3), RW)));
    assert_eq!(map(&fx, h, 0xffff_8000_0000_0000, 0, P), Ok(0));
    assert_eq!(tr(&fx, 0, 0x8000_0000_0000), Some((Some(0), RW)));
    // UNMAP takes the mapping starting there, whole; again ENOENT; not at its start ENOENT.
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, 0, V + P, 0, 0), Err(ENOENT));
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, 0, V, 0, 0), Ok(0));
    assert_eq!(tr(&fx, 0, V), None);
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, 0, V, 0, 0), Err(ENOENT));
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, 0, 0xffff_8000_0000_0000, 0, 0), Ok(0));
    assert_eq!(maps(&fx, 0).len(), 1);
    // Another object's handle does not unmap it (its own bo_va only).
    let o = fx.create(P, GTT, 0).unwrap();
    assert_eq!(va(&fx, 0, o, AMDGPU_VA_OP_UNMAP, 0, V + 8 * P, 0, 0), Err(ENOENT));
    // The other file's VM is its own.
    assert!(maps(&fx, 1).is_empty());
    assert!(fx.f(1).drm_file().vm.backend_vm().is_none());
}

#[test]
fn clear_splits_and_replace() {
    let fx = Fx::new();
    let a = fx.create(8 * P, GTT, 0).unwrap();
    let b = fx.create(2 * P, VRAM, 0).unwrap();
    assert_eq!(map(&fx, a, V, 0, 8 * P), Ok(0));
    // CLEAR [V+2P, V+4P): the pieces keep their offsets (amdgpu_vm.c:1740).
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_CLEAR, 0, V + 2 * P, 0, 2 * P), Ok(0));
    assert_eq!(maps(&fx, 0), [info(0, 2 * P, V, RW), info(4 * P, 4 * P, V + 4 * P, RW)]);
    assert_eq!(tr(&fx, 0, V + 2 * P), None);
    assert_eq!(tr(&fx, 0, V + 5 * P), Some((Some(5), RW)));
    // A CLEAR over nothing and of size 0 are fine.
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_CLEAR, 0, V + 64 * P, 0, P), Ok(0));
    // As on CachyOS's kernel: unaligned and empty CLEARs are EINVAL.
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_CLEAR, 0, V, 0, 0), Err(EINVAL));
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_CLEAR, 0, V + 8, 0, P), Err(EINVAL));
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_CLEAR, 0, V, 0, P + 8), Err(EINVAL));
    // UNMAP of the second piece by its new start.
    assert_eq!(va(&fx, 0, a, AMDGPU_VA_OP_UNMAP, 0, V + 4 * P, 0, 0), Ok(0));
    assert_eq!(maps(&fx, 0), [info(0, 2 * P, V, RW)]);
    // REPLACE with b in the middle of a fresh mapping of a.
    assert_eq!(map(&fx, a, V + 16 * P, 0, 4 * P), Ok(0));
    let x = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE | AMDGPU_VM_MTYPE_UC;
    assert_eq!(va(&fx, 0, b, AMDGPU_VA_OP_REPLACE, x, V + 17 * P, 0, 2 * P), Ok(0));
    assert_eq!(
        maps(&fx, 0),
        [info(0, 2 * P, V, RW), info(0, P, V + 16 * P, RW), info(0, 2 * P, V + 17 * P, x), info(3 * P, P, V + 19 * P, RW)]
    );
    assert_eq!(tr(&fx, 0, V + 18 * P), Some((Some(1), x)));
    assert_eq!(tr(&fx, 0, V + 19 * P), Some((Some(3), RW)));
    // REPLACE over nothing is a MAP; its own checks hold.
    assert_eq!(va(&fx, 0, b, AMDGPU_VA_OP_REPLACE, RW, V + 32 * P, P, 2 * P), Err(EINVAL));
    assert_eq!(va(&fx, 0, b, AMDGPU_VA_OP_REPLACE, RW, V + 32 * P, 0, 2 * P), Ok(0));
    assert_eq!(pieces(&fx, 0), maps(&fx, 0).len(), "the fake holds what the map says");
}

#[test]
fn prt_mappings() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_MAP, AMDGPU_VM_PAGE_PRT, V, 0, 1 << 30), Ok(0));
    assert_eq!(tr(&fx, 0, V + (1 << 29)), Some((None, AMDGPU_VM_PAGE_PRT)));
    let vm = fx.f(0).drm_file().vm.backend_vm().unwrap();
    assert_eq!(vm.as_fake().unwrap().walk(V + (1 << 29)), TableWalk::Prt, "a PRT range in the GFX 11 tables");
    assert_eq!(vm.as_fake().unwrap().walk(V + (1 << 30)), TableWalk::Hole);
    // A mapping of an object over PRT: REPLACE binds a page of a sparse resource.
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_REPLACE, RW, V + P, 0, P), Ok(0));
    assert_eq!(tr(&fx, 0, V + P), Some((Some(0), RW)));
    assert_eq!(tr(&fx, 0, V + 2 * P), Some((None, AMDGPU_VM_PAGE_PRT)));
    // UNMAP of PRT needs the PRT flag (the PRT bo_va); with an object's handle it is ENOENT.
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, 0, V, 0, 0), Err(ENOENT));
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_UNMAP, AMDGPU_VM_PAGE_PRT, V, 0, 0), Ok(0));
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_UNMAP, AMDGPU_VM_PAGE_PRT, V + 2 * P, 0, 0), Ok(0));
    assert_eq!(maps(&fx, 0), [info(0, P, V + P, RW)]);
    assert_eq!(tr(&fx, 0, V + 3 * P), None);
}

#[test]
fn delay_update_queues() {
    let fx = Fx::new();
    let h = fx.create(2 * P, GTT, 0).unwrap();
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_MAP, RW | AMDGPU_VM_DELAY_UPDATE, V, 0, P), Ok(0));
    let vm = &fx.f(0).drm_file().vm;
    assert_eq!(vm.queued(), 1);
    assert_eq!(tr(&fx, 0, V), None, "not in the tables yet");
    assert_eq!(maps(&fx, 0), [info(0, P, V, RW)], "but in the interval map (no DELAY bit kept)");
    assert_eq!(map(&fx, h, V + P, P, P), Ok(0));
    assert_eq!(vm.queued(), 0);
    assert_eq!(tr(&fx, 0, V), Some((Some(0), RW)));
    assert_eq!(tr(&fx, 0, V + P), Some((Some(1), RW)));
    // CS (DR-8) flushes too.
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, AMDGPU_VM_DELAY_UPDATE, V, 0, 0), Ok(0));
    assert!(tr(&fx, 0, V).is_some());
    assert_eq!(vm.flush(), Ok(()));
    assert_eq!(tr(&fx, 0, V), None);
}

#[test]
fn both_struct_sizes() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    let a = drm_amdgpu_gem_va { handle: h, operation: AMDGPU_VA_OP_MAP, flags: RW, va_address: V, map_size: P, ..drm_amdgpu_gem_va::ZERO };
    fx.m.put(0, &a);
    // A 6.6 caller's buffer ends at 40 bytes: what follows must not be read.
    for o in [40, 48, 56] {
        fx.m.put(o, &u64::MAX);
    }
    assert_eq!(fx.io(0, VA40), Ok(0));
    assert_eq!(tr(&fx, 0, V), Some((Some(0), RW)));
    // The 64-byte form with an unknown out-syncobj: ENOENT before anything changes.
    let a = drm_amdgpu_gem_va { va_address: V + P, vm_timeline_syncobj_out: 99, ..a };
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_GEM_VA), Err(ENOENT));
    assert_eq!(maps(&fx, 0).len(), 1);
    // Binary and timeline out-syncobjs get a signaled fence (the fake writes the tables at once).
    let file = fx.f(0).drm_file();
    let (bin, tl) = (Syncobj::new(false), Syncobj::new(false));
    let (hb, ht) = (file.syncobj_insert(bin.clone()).unwrap(), file.syncobj_insert(tl.clone()).unwrap());
    fx.m.put(0, &drm_amdgpu_gem_va { vm_timeline_syncobj_out: hb, ..a });
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_GEM_VA), Ok(0));
    assert!(bin.fence().is_some_and(|f| crate::fence::is_signaled(&f)));
    fx.m.put(0, &drm_amdgpu_gem_va { operation: AMDGPU_VA_OP_UNMAP, vm_timeline_syncobj_out: ht, vm_timeline_point: 7, ..a });
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_GEM_VA), Ok(0));
    let f = tl.fence().unwrap();
    assert!(crate::fence::is_signaled(&f));
    assert_eq!(f.seqno(), 7);
}

#[test]
fn close_takes_the_bo_va() {
    let mut fx = Fx::new();
    let h = fx.create(4 * P, GTT, 0).unwrap();
    let w = Arc::downgrade(&fx.bo(0, h));
    assert_eq!(map(&fx, h, V, 0, 4 * P), Ok(0));
    assert_eq!(map(&fx, h, V + 8 * P, 0, P), Ok(0));
    assert_eq!(fx.gpu.bo_usage(), (0, 4 * P));
    // A second handle of the same object in the same file (GEM_OPEN by name): one bo_va, two references.
    let p = fx.open_primary(true);
    let hp = fx.create_in(p, 2 * P, 0, GTT, 0).unwrap();
    let n = fx.flink(p, hp).unwrap();
    let (h2, _) = fx.open_name(p, n).unwrap();
    assert_ne!(h2, hp);
    assert_eq!(va(&fx, p, hp, AMDGPU_VA_OP_MAP, RW, V, 0, 2 * P), Ok(0));
    assert_eq!(fx.f(p).drm_file().vm.bo_vas(), 1);
    assert_eq!(fx.close(p, hp), Ok(0));
    assert_eq!(maps(&fx, p).len(), 1, "the other handle keeps the bo_va");
    assert_eq!(va(&fx, p, h2, AMDGPU_VA_OP_UNMAP, 0, V, 0, 0), Ok(0));
    assert_eq!(fx.close(p, h2), Ok(0));
    assert_eq!(fx.f(p).drm_file().vm.bo_vas(), 0);
    // The last handle: mappings, tables and the memory go (amdgpu_gem_object_close).
    assert_eq!(fx.close(0, h), Ok(0));
    assert!(maps(&fx, 0).is_empty());
    assert_eq!(pieces(&fx, 0), 0);
    assert!(w.upgrade().is_none());
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
    // The address is free again.
    let g = fx.create(P, GTT, 0).unwrap();
    assert_eq!(map(&fx, g, V, 0, P), Ok(0));
}

#[test]
fn mapped_memory_outlives_the_object_until_unmapped() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    let mem = Arc::downgrade(fx.bo(0, h).memory());
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_MAP, RW | AMDGPU_VM_DELAY_UPDATE, V, 0, P), Ok(0));
    assert_eq!(map(&fx, h, V + P, 0, P), Ok(0));
    // UNMAPs still queued keep the memory: the GPU can reach it until the jobs ran.
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, AMDGPU_VM_DELAY_UPDATE, V, 0, 0), Ok(0));
    assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, AMDGPU_VM_DELAY_UPDATE, V + P, 0, 0), Ok(0));
    assert_eq!(fx.f(0).drm_file().vm.queued(), 2);
    assert!(mem.upgrade().is_some());
    assert_eq!(fx.close(0, h), Ok(0));
    assert!(mem.upgrade().is_none(), "the last close of the object runs the queued unmaps (amdgpu_vm_clear_freed)");
}

#[test]
fn last_close_releases_the_vm() {
    let mut fx = Fx::new();
    let h = fx.create(4 * P, VRAM, 0).unwrap();
    let w = Arc::downgrade(&fx.bo(0, h));
    let mem = Arc::downgrade(fx.bo(0, h).memory());
    assert_eq!(map(&fx, h, V, 0, 4 * P), Ok(0));
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_MAP, AMDGPU_VM_PAGE_PRT | AMDGPU_VM_DELAY_UPDATE, V + (1 << 30), 0, P), Ok(0));
    assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_RESERVE_VMID, 0), Ok(0));
    assert!(fx.gpu.shared().reserved_vmid_owner().is_some());
    let vm = Arc::downgrade(&fx.f(0).drm_file().vm.backend_vm().unwrap());
    fx.files[0] = None;
    assert!(w.upgrade().is_none());
    assert!(mem.upgrade().is_none());
    assert!(vm.upgrade().is_none(), "the backend's VM went with the file");
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
    assert_eq!(fx.gpu.shared().reserved_vmid_owner(), None, "vm_fini gives the VMID back");
    assert_eq!(vm_op(&fx, 1, AMDGPU_VM_OP_RESERVE_VMID, 0), Ok(0), "another file gets it then");
}

#[test]
fn vmid_reserve() {
    let fx = Fx::new();
    let s = fx.gpu.shared();
    let me = fx.f(0).drm_file().client_id();
    assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_RESERVE_VMID, 1), Err(EINVAL), "no flags defined");
    assert_eq!(vm_op(&fx, 0, 0, 0), Err(EINVAL));
    assert_eq!(vm_op(&fx, 0, 3, 0), Err(EINVAL));
    assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_UNRESERVE_VMID, 0), Ok(0), "unreserve without reserve is fine");
    assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_RESERVE_VMID, 0), Ok(0));
    assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_RESERVE_VMID, 0), Ok(0), "again by the holder");
    assert_eq!(s.reserved_vmid_owner(), Some(me));
    assert!(fx.f(0).drm_file().vm.vmid_reserved());
    // One holder at a time (CachyOS's kernel).
    assert_eq!(vm_op(&fx, 1, AMDGPU_VM_OP_RESERVE_VMID, 0), Err(ENOENT));
    assert_eq!(vm_op(&fx, 1, AMDGPU_VM_OP_UNRESERVE_VMID, 0), Ok(0));
    assert_eq!(s.reserved_vmid_owner(), Some(me), "a non-holder's unreserve changes nothing");
    assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_UNRESERVE_VMID, 0), Ok(0));
    assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_UNRESERVE_VMID, 0), Ok(0));
    assert_eq!(s.reserved_vmid_owner(), None);
    assert_eq!(vm_op(&fx, 1, AMDGPU_VM_OP_RESERVE_VMID, 0), Ok(0));
    assert_eq!(s.reserved_vmid_owner(), Some(fx.f(1).drm_file().client_id()));
    // The union's 8 bytes are copied back unchanged (DRM_IOWR, `out` unused by these ops).
    assert_eq!(fx.m.take::<drm_amdgpu_vm_in>(0), drm_amdgpu_vm_in { op: AMDGPU_VM_OP_RESERVE_VMID, flags: 0 });
}

#[test]
fn fdinfo_memory_lines() {
    let fx = Fx::new();
    fx.create(4 * P, GTT, 0).unwrap();
    fx.create(2 * P, VRAM, AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED as u64).unwrap();
    let mut s = std::string::String::new();
    fx.f(0).show_fdinfo(&mut s);
    for l in [
        "drm-memory-vram:\t8 KiB\n",
        "drm-memory-gtt: \t16 KiB\n",
        "drm-memory-cpu: \t0 KiB\n",
        "amd-memory-visible-vram:\t8 KiB\n",
        "amd-evicted-vram:\t0 KiB\n",
        "amd-requested-vram:\t8 KiB\n",
        "amd-requested-gtt:\t16 KiB\n",
    ] {
        assert!(s.contains(l), "{l:?} in {s}");
    }
    let mut t = std::string::String::new();
    fx.f(1).show_fdinfo(&mut t);
    assert!(t.contains("drm-memory-gtt: \t0 KiB\n"), "per file");
}

#[test]
fn concurrent_maps_and_closes() {
    let fx = Arc::new(Fx::new());
    let hs: Vec<u32> = (0..8).map(|_| fx.create(2 * P, GTT, 0).unwrap()).collect();
    let ts: Vec<_> = hs
        .iter()
        .enumerate()
        .map(|(i, &h)| {
            let fx = fx.clone();
            thread::spawn(move || {
                let at = V + (i as u64) * 0x100_0000;
                let m = crate::dr6_tests::Mem::new();
                for r in 0..50u64 {
                    let a = drm_amdgpu_gem_va { handle: h, operation: AMDGPU_VA_OP_MAP, flags: RW, va_address: at + r * 2 * P, map_size: 2 * P, ..drm_amdgpu_gem_va::ZERO };
                    m.put(0, &a);
                    assert_eq!(fx.f(0).ioctl_with(DRM_IOCTL_AMDGPU_GEM_VA, BASE, &m, ME, None), Ok(0));
                }
            })
        })
        .collect();
    for t in ts {
        t.join().unwrap();
    }
    assert_eq!(maps(&fx, 0).len(), 8 * 50);
    assert_eq!(pieces(&fx, 0), 8 * 50, "every job reached the fake, none lost or doubled");
    for &h in &hs {
        fx.close(0, h).unwrap();
    }
    assert!(maps(&fx, 0).is_empty());
    assert_eq!(pieces(&fx, 0), 0);
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
}

// ---- fixer: VMID race, the flush's bounds, refused jobs, a random model, EFAULT and caller sizes

#[test]
fn vmid_reserve_race_in_one_file() {
    let fx = Arc::new(Fx::new());
    let me = fx.f(0).drm_file().client_id();
    for _ in 0..200 {
        assert_eq!(vm_op(&fx, 0, AMDGPU_VM_OP_UNRESERVE_VMID, 0), Ok(0));
        let ts: Vec<_> = (0..2)
            .map(|_| {
                let fx = fx.clone();
                thread::spawn(move || {
                    let m = crate::dr6_tests::Mem::new();
                    m.put(0, &drm_amdgpu_vm_in { op: AMDGPU_VM_OP_RESERVE_VMID, flags: 0 });
                    fx.f(0).ioctl_with(DRM_IOCTL_AMDGPU_VM, BASE, &m, ME, None)
                })
            })
            .collect();
        for t in ts {
            assert_eq!(t.join().unwrap(), Ok(0), "both threads of the holder succeed");
        }
        assert_eq!(fx.gpu.shared().reserved_vmid_owner(), Some(me));
        assert!(fx.f(0).drm_file().vm.vmid_reserved());
    }
}

/// A backend that records the jobs it ran and refuses what `refuse` names.
struct Picky {
    ran: std::sync::Mutex<Vec<(u8, u64)>>,
    refuse: std::sync::Mutex<Option<(u8, vfs::Errno)>>,
    /// Queues another job into this VM while it runs (another thread's GEM_VA).
    feed: std::sync::Mutex<Option<Arc<crate::vm::FileVm>>>,
}

fn kind(op: &crate::gpu::PtOp) -> u8 {
    match op {
        crate::gpu::PtOp::Map { .. } => b'M',
        crate::gpu::PtOp::Prt { .. } => b'P',
        crate::gpu::PtOp::Clear { .. } => b'C',
    }
}

impl crate::gpu::GpuVm for Picky {
    fn update(&self, ops: &[crate::gpu::PtOp]) -> crate::gpu::VmUpdate {
        if let Some(vm) = self.feed.lock().unwrap().as_ref() {
            let n = self.ran.lock().unwrap().len() as u64;
            crate::vm::map(vm, crate::vm::Owner::Prt, None, V + (n + 100) * P, 0, P, 0, false).unwrap();
        }
        let mut done = 0;
        let mut error = None;
        for op in ops {
            if let Some((k, e)) = *self.refuse.lock().unwrap()
                && k == kind(op)
            {
                error = Some(e);
                break;
            }
            self.ran.lock().unwrap().push((kind(op), op.range().0));
            done += 1;
        }
        crate::gpu::VmUpdate { done, error, fence: None }
    }
}

fn picky() -> Arc<Picky> {
    Arc::new(Picky { ran: Default::default(), refuse: Default::default(), feed: Default::default() })
}

#[test]
fn refused_clear_is_never_dropped() {
    use crate::vm::{clear, map as vmap, FileVm, Owner};
    let vm = FileVm::new();
    let b = picky();
    vm.set_backend_for_test(b.clone());
    vmap(&vm, Owner::Prt, None, V, 0, 2 * P, 0, false).unwrap();
    assert_eq!(vm.flush(), Ok(()));
    *b.refuse.lock().unwrap() = Some((b'C', EIO));
    clear(&vm, V, 2 * P).unwrap();
    assert_eq!(vm.flush(), Err(EIO));
    assert_eq!(vm.queued(), 1, "the Clear stays: the backend may still reach the range");
    vmap(&vm, Owner::Prt, None, V, 0, P, 0, false).unwrap();
    assert_eq!(vm.flush(), Err(EIO), "every flush fails while the Clear has not run");
    assert_eq!(vm.queued(), 2, "and nothing behind it runs");
    *b.refuse.lock().unwrap() = None;
    assert_eq!(vm.flush(), Ok(()));
    assert_eq!(vm.queued(), 0);
    assert_eq!(*b.ran.lock().unwrap(), [(b'P', V), (b'C', V), (b'P', V)], "in queue order");
    // A refused Prt (or Map) job is dropped: the range faults, the queue moves on.
    *b.refuse.lock().unwrap() = Some((b'P', EINVAL));
    vmap(&vm, Owner::Prt, None, V + 8 * P, 0, P, 0, false).unwrap();
    assert_eq!(vm.flush(), Err(EINVAL));
    assert_eq!(vm.queued(), 0);
    // Memory: retried, never dropped.
    *b.refuse.lock().unwrap() = Some((b'P', ENOMEM));
    vmap(&vm, Owner::Prt, None, V + 16 * P, 0, P, 0, false).unwrap();
    assert_eq!(vm.flush(), Err(ENOMEM));
    assert_eq!(vm.queued(), 1);
    *b.refuse.lock().unwrap() = None;
    assert_eq!(vm.flush(), Ok(()));
    assert_eq!(vm.queued(), 0);
}

#[test]
fn flush_ends_while_others_queue() {
    use crate::vm::{map as vmap, FileVm, Owner};
    let vm = Arc::new(FileVm::new());
    let b = picky();
    vm.set_backend_for_test(b.clone());
    *b.feed.lock().unwrap() = Some(vm.clone());
    vmap(&vm, Owner::Prt, None, V, 0, P, 0, false).unwrap();
    // Every update queues one more job; the flush still returns, leaving that job to the next flush.
    assert_eq!(vm.flush(), Ok(()));
    assert_eq!(vm.queued(), 1);
    assert_eq!(vm.flush(), Ok(()));
    assert_eq!(vm.queued(), 1);
    *b.feed.lock().unwrap() = None;
    assert_eq!(vm.flush(), Ok(()));
    assert_eq!(vm.queued(), 0);
    assert_eq!(b.ran.lock().unwrap().len(), 3);
}

/// Seeded random MAP/UNMAP/CLEAR/REPLACE in a 64-page window against a page model: answers, mappings (with
/// the split pieces' offsets), the fake's translations and table walks, and nothing left at the end.
#[test]
fn random_ops_match_a_model() {
    use std::collections::BTreeMap;
    const W: u64 = 64;
    const OBJ: u64 = 16;
    let fx = Fx::new();
    let hs: Vec<u32> = (0..3).map(|_| fx.create(OBJ * P, GTT, 0).unwrap()).collect();
    let bos: Vec<_> = hs.iter().map(|&h| fx.bo(0, h)).collect();
    // start page -> (last page, object, offset in pages)
    let mut model: BTreeMap<u64, (u64, usize, u64)> = BTreeMap::new();
    let clear_model = |m: &mut BTreeMap<u64, (u64, usize, u64)>, s: u64, e: u64| {
        let hit: Vec<u64> = m.iter().filter(|(ms, v)| **ms <= e && v.0 >= s).map(|(&ms, _)| ms).collect();
        for ms in hit {
            let (ml, o, off) = m.remove(&ms).unwrap();
            if ms < s {
                m.insert(ms, (s - 1, o, off));
            }
            if ml > e {
                m.insert(e + 1, (ml, o, off + (e + 1 - ms)));
            }
        }
    };
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut rnd = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let (mut ok, mut most) = ([0u32; 4], 0);
    for step in 0..3000 {
        let r = rnd();
        let o = (r >> 8) as usize % 3;
        let s = (r >> 16) % W;
        let n = (1 + (r >> 24) % 8).min(W - s);
        let off = (r >> 32) % (OBJ + 2);
        let e = s + n - 1;
        let at = V + s * P;
        let (got, want) = match r % 4 {
            0 => {
                let got = va(&fx, 0, hs[o], AMDGPU_VA_OP_MAP, RW, at, off * P, n * P);
                let want = if off + n > OBJ || model.iter().any(|(&ms, &(ml, _, _))| ms <= e && ml >= s) {
                    Err(EINVAL)
                } else {
                    model.insert(s, (e, o, off));
                    Ok(0)
                };
                (got, want)
            }
            1 => {
                let got = va(&fx, 0, hs[o], AMDGPU_VA_OP_UNMAP, 0, at, 0, 0);
                let want = match model.get(&s) {
                    Some(&(_, mo, _)) if mo == o => {
                        model.remove(&s);
                        Ok(0)
                    }
                    _ => Err(ENOENT),
                };
                (got, want)
            }
            2 => {
                let got = va(&fx, 0, 0, AMDGPU_VA_OP_CLEAR, 0, at, 0, n * P);
                clear_model(&mut model, s, e);
                (got, Ok(0))
            }
            _ => {
                let got = va(&fx, 0, hs[o], AMDGPU_VA_OP_REPLACE, RW, at, off * P, n * P);
                let want = if off + n > OBJ {
                    Err(EINVAL)
                } else {
                    clear_model(&mut model, s, e);
                    model.insert(s, (e, o, off));
                    Ok(0)
                };
                (got, want)
            }
        };
        assert_eq!(got, want, "step {step}: op {} obj {o} pages {s}+{n} offset {off}", r % 4);
        if got.is_ok() {
            ok[(r % 4) as usize] += 1;
        }
        most = most.max(model.len());
        let want_maps: Vec<MapInfo> = model.iter().map(|(&ms, &(ml, _, off))| info(off * P, (ml - ms + 1) * P, V + ms * P, RW)).collect();
        assert_eq!(maps(&fx, 0), want_maps, "step {step}");
        if step % 16 == 0 {
            let vm = fx.f(0).drm_file().vm.backend_vm().unwrap();
            let fake = vm.as_fake().unwrap();
            for p in 0..W {
                let hit = model.range(..=p).next_back().filter(|(_, v)| v.0 >= p);
                let t = fake.translate(V + p * P);
                match hit {
                    Some((&ms, &(_, o, off))) => {
                        let t = t.unwrap_or_else(|| panic!("step {step}: page {p} not translated"));
                        let Held::Mem { mem, .. } = &t.held else { panic!("step {step}: page {p} PRT") };
                        assert!(std::ptr::addr_eq(Arc::as_ptr(mem), Arc::as_ptr(bos[o].memory())), "step {step}: page {p} other object");
                        let page = off + (p - ms);
                        assert_eq!(t.page, page, "step {step}: page {p}");
                        if let Some(phys) = bos[o].memory().page_phys(page) {
                            assert!(matches!(fake.walk(V + p * P), TableWalk::Page { phys: w, .. } if w == phys), "step {step}: walk of page {p}");
                        }
                    }
                    None => {
                        assert!(t.is_none(), "step {step}: page {p} still translated");
                        assert_eq!(fake.walk(V + p * P), TableWalk::Hole, "step {step}: page {p}");
                    }
                }
            }
        }
    }
    assert!(ok.iter().all(|&k| k > 30) && most > 4, "the sequence exercised every operation: {ok:?}, up to {most} mappings");
    assert_eq!(fx.f(0).drm_file().vm.queued(), 0);
    drop(bos);
    for &h in &hs {
        fx.close(0, h).unwrap();
    }
    assert!(maps(&fx, 0).is_empty());
    assert_eq!(pieces(&fx, 0), 0);
    assert_eq!(fx.f(0).drm_file().vm.queued(), 0);
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
}

#[test]
fn efault_and_caller_sizes() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    let end = BASE + 8192;
    let at = |n: u32| ioc::ioc(ioc::IOC_WRITE, DRM_IOCTL_BASE, 0x48, n);
    for cmd in [DRM_IOCTL_AMDGPU_GEM_VA, VA40, DRM_IOCTL_AMDGPU_VM] {
        assert_eq!(fx.f(0).ioctl_with(cmd, 0, &fx.m, ME, None), Err(EFAULT));
    }
    // 64 bytes asked where only 40 are readable: EFAULT; the 40-byte form there reads what it may.
    assert_eq!(fx.f(0).ioctl_with(DRM_IOCTL_AMDGPU_GEM_VA, end - 40, &fx.m, ME, None), Err(EFAULT));
    assert_eq!(fx.f(0).ioctl_with(VA40, end - 40, &fx.m, ME, None), Err(EINVAL), "zeros: address 0");
    assert!(maps(&fx, 0).is_empty());
    // Callers of 48 and 56 bytes: the fields past their size are not read.
    let a = drm_amdgpu_gem_va {
        handle: h,
        operation: AMDGPU_VA_OP_MAP,
        flags: RW,
        va_address: V,
        map_size: P,
        vm_timeline_point: 5,
        vm_timeline_syncobj_out: 99,
        num_syncobj_handles: 3,
        ..drm_amdgpu_gem_va::ZERO
    };
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, at(48)), Ok(0));
    assert_eq!(maps(&fx, 0).len(), 1);
    fx.m.put(0, &drm_amdgpu_gem_va { operation: AMDGPU_VA_OP_UNMAP, num_syncobj_handles: 0, ..a });
    assert_eq!(fx.io(0, at(56)), Err(ENOENT), "the out-syncobj is inside 56 bytes");
    assert_eq!(maps(&fx, 0).len(), 1, "refused before anything changed");
    fx.m.put(0, &drm_amdgpu_gem_va { operation: AMDGPU_VA_OP_UNMAP, vm_timeline_syncobj_out: 0, num_syncobj_handles: 0, ..a });
    assert_eq!(fx.io(0, at(56)), Ok(0));
    assert!(maps(&fx, 0).is_empty());
}

/// The bottom the device reports (64 KiB), not Linux 6.6's fixed 2 MiB (CachyOS answers the same, abi-drm-va).
#[test]
fn bottom_is_the_reported_one() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    assert_eq!(map(&fx, h, 0x1_0000 - P, 0, P), Err(EINVAL));
    for at in [0x1_0000, 0x1f_f000] {
        assert_eq!(map(&fx, h, at, 0, P), Ok(0));
        assert_eq!(va(&fx, 0, h, AMDGPU_VA_OP_UNMAP, 0, at, 0, P), Ok(0));
    }
    // PRT ignores the object but not the offset's overflow (amdgpu_vm.c:1532; CachyOS: EINVAL).
    assert_eq!(va(&fx, 0, 0, AMDGPU_VA_OP_MAP, AMDGPU_VM_PAGE_PRT, V, !0xfff, P), Err(EINVAL));
}
