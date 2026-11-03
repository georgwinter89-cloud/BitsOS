//! DR-6 host tests: GEM_CREATE's checks, shapes and retries against the fake for any carve-out size, the create
//! info, GEM_MMAP offsets and the render node's mapping rules, GEM_WAIT_IDLE with ring fences, GEM_METADATA,
//! GEM_OP, GEM_USERPTR's refusals, PRIME export/import (one buffer per object, the same handle back, the same
//! object in a second file, a foreign udmabuf), reservation objects shared with the dma-buf, and nothing left
//! behind when handles, fds and files go. Expected values are Linux 6.6's (CachyOS 7.3 where they agree; the ABI
//! program `abi-drm-gem` checks those on the real kernel).

use crate::device;
use crate::dispatch::Caller;
use crate::dma_buf::{self, DmaBuf};
use crate::dr2_tests::MINORS;
use crate::fake::{FakeGpu, FakeMemory};
use crate::file::MinorKind;
use crate::gem::{self, Bo, MMAP_START_PG};
use crate::gpu::{BoPlace, GpuDevice};
use crate::node::{open_device, OpenDrm};
use crate::uapi::*;
use crate::wire::Wire;
use core::sync::atomic::{AtomicBool, Ordering};
use linux::errno::*;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use std::vec::Vec;
use std::{thread, vec};
use vfs::fd::FdTable;
use vfs::File;

pub(crate) const BASE: u64 = 0x10000;
pub(crate) const P: u64 = 4096;
pub(crate) const ME: Caller = Caller { pid: 42, sys_admin: false, sys_nice: false };
const GTT: u64 = AMDGPU_GEM_DOMAIN_GTT as u64;
const VRAM: u64 = AMDGPU_GEM_DOMAIN_VRAM as u64;
const CPU: u64 = AMDGPU_GEM_DOMAIN_CPU as u64;
const CAR: u64 = AMDGPU_GEM_CREATE_CPU_ACCESS_REQUIRED as u64;
const NO_CPU: u64 = AMDGPU_GEM_CREATE_NO_CPU_ACCESS as u64;
const USWC: u64 = AMDGPU_GEM_CREATE_CPU_GTT_USWC as u64;
const AV: u64 = AMDGPU_GEM_CREATE_VM_ALWAYS_VALID as u64;
const EXPLICIT: u64 = AMDGPU_GEM_CREATE_EXPLICIT_SYNC as u64;

pub(crate) struct Mem {
    b: Mutex<Vec<u8>>,
    pub(crate) kill: AtomicBool,
}

impl Mem {
    pub(crate) fn new() -> Mem {
        Mem { b: Mutex::new(vec![0; 8192]), kill: AtomicBool::new(false) }
    }
    pub(crate) fn put<T: Wire>(&self, off: usize, v: &T) {
        let mut b = vec![0u8; T::SIZE];
        v.encode(&mut b).unwrap();
        self.b.lock().unwrap()[off..off + T::SIZE].copy_from_slice(&b);
    }
    pub(crate) fn take<T: Wire>(&self, off: usize) -> T {
        T::decode(&self.b.lock().unwrap()[off..off + T::SIZE]).unwrap()
    }
}

impl vfs::sys::UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let b = self.b.lock().unwrap();
        let Some(s) = addr.checked_sub(BASE).and_then(|s| usize::try_from(s).ok()) else { return false };
        if s + buf.len() > b.len() {
            return false;
        }
        buf.copy_from_slice(&b[s..s + buf.len()]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let mut b = self.b.lock().unwrap();
        let Some(s) = addr.checked_sub(BASE).and_then(|s| usize::try_from(s).ok()) else { return false };
        if s + data.len() > b.len() {
            return false;
        }
        b[s..s + data.len()].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
    fn interrupted(&self) -> bool {
        self.kill.load(Ordering::SeqCst)
    }
}

pub(crate) struct Fx {
    pub(crate) gpu: Arc<FakeGpu>,
    dev: Arc<device::Device>,
    pub(crate) files: Vec<Option<Arc<OpenDrm>>>,
    pub(crate) fds: Arc<FdTable>,
    pub(crate) m: Mem,
}

impl Fx {
    pub(crate) fn new() -> Fx {
        Fx::with(FakeGpu::radeon_780m())
    }
    pub(crate) fn with(g: FakeGpu) -> Fx {
        let gpu = Arc::new(g);
        let dev = {
            let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
            device::register(gpu.clone()).unwrap()
        };
        let files = (0..2).map(|_| Some(open_device(dev.clone(), MinorKind::Render, false).unwrap())).collect();
        Fx { gpu, dev, files, fds: Arc::new(FdTable::new()), m: Mem::new() }
    }
    pub(crate) fn f(&self, k: usize) -> &OpenDrm {
        self.files[k].as_ref().unwrap()
    }
    pub(crate) fn io(&self, k: usize, cmd: u32) -> Result<i64, vfs::Errno> {
        self.f(k).ioctl_with(cmd, BASE, &self.m, ME, Some(self.fds.clone()))
    }
    pub(crate) fn create_in(&self, k: usize, size: u64, align: u64, domains: u64, flags: u64) -> Result<u32, vfs::Errno> {
        let mut c = drm_amdgpu_gem_create::ZERO;
        c.set_in(&drm_amdgpu_gem_create_in { bo_size: size, alignment: align, domains, domain_flags: flags });
        self.m.put(0, &c);
        self.io(k, DRM_IOCTL_AMDGPU_GEM_CREATE)?;
        Ok(self.m.take::<drm_amdgpu_gem_create>(0).out().handle)
    }
    pub(crate) fn create(&self, size: u64, domains: u64, flags: u64) -> Result<u32, vfs::Errno> {
        self.create_in(0, size, 0, domains, flags)
    }
    pub(crate) fn info(&self, k: usize, h: u32) -> Result<drm_amdgpu_gem_create_in, vfs::Errno> {
        self.m.put(0, &drm_amdgpu_gem_op { handle: h, op: AMDGPU_GEM_OP_GET_GEM_CREATE_INFO, value: BASE + 1024 });
        self.io(k, DRM_IOCTL_AMDGPU_GEM_OP)?;
        Ok(self.m.take(1024))
    }
    pub(crate) fn op(&self, h: u32, op: u32, value: u64) -> Result<i64, vfs::Errno> {
        self.m.put(0, &drm_amdgpu_gem_op { handle: h, op, value });
        self.io(0, DRM_IOCTL_AMDGPU_GEM_OP)
    }
    pub(crate) fn offset(&self, k: usize, h: u32) -> (Result<i64, vfs::Errno>, u64) {
        let mut a = drm_amdgpu_gem_mmap::ZERO;
        a.set_in(&drm_amdgpu_gem_mmap_in { handle: h, _pad: 0 });
        self.m.put(0, &a);
        let r = self.io(k, DRM_IOCTL_AMDGPU_GEM_MMAP);
        (r, self.m.take::<drm_amdgpu_gem_mmap>(0).out().addr_ptr)
    }
    pub(crate) fn wait(&self, h: u32, timeout: u64) -> Result<u32, vfs::Errno> {
        let mut a = drm_amdgpu_gem_wait_idle::ZERO;
        a.set_in(&drm_amdgpu_gem_wait_idle_in { handle: h, flags: 0, timeout });
        self.m.put(0, &a);
        self.io(0, DRM_IOCTL_AMDGPU_GEM_WAIT_IDLE)?;
        Ok(self.m.take::<drm_amdgpu_gem_wait_idle>(0).out().status)
    }
    pub(crate) fn close(&self, k: usize, h: u32) -> Result<i64, vfs::Errno> {
        self.m.put(0, &drm_gem_close { handle: h, pad: 0 });
        self.io(k, DRM_IOCTL_GEM_CLOSE)
    }
    pub(crate) fn to_fd(&self, k: usize, h: u32, flags: u32) -> Result<i32, vfs::Errno> {
        self.m.put(0, &drm_prime_handle { handle: h, flags, fd: -1 });
        self.io(k, DRM_IOCTL_PRIME_HANDLE_TO_FD)?;
        Ok(self.m.take::<drm_prime_handle>(0).fd)
    }
    pub(crate) fn to_handle(&self, k: usize, fd: i32) -> Result<u32, vfs::Errno> {
        self.m.put(0, &drm_prime_handle { handle: 0, flags: 0, fd });
        self.io(k, DRM_IOCTL_PRIME_FD_TO_HANDLE)?;
        Ok(self.m.take::<drm_prime_handle>(0).handle)
    }
    pub(crate) fn bo(&self, k: usize, h: u32) -> Arc<Bo> {
        self.f(k).drm_file().bo(h).unwrap()
    }
    pub(crate) fn buf(&self, fd: i32) -> Arc<DmaBuf> {
        dma_buf::as_dma_buf(&self.fds.get(fd as usize).unwrap().file).unwrap()
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.files.clear();
        let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
        device::unregister(&self.dev);
    }
}

/// A udmabuf of `pages` pages over a fresh memfd in the fixture's fd table: (memfd, buffer fd).
fn udmabuf(fx: &Fx, pages: u64) -> (Arc<vfs::fd::OpenFile>, i32) {
    let f = vfs::memfd::create(b"dr6", vfs::memfd::MFD_ALLOW_SEALING, vfs::inode::Cred::ROOT).unwrap();
    f.inode().unwrap().truncate(pages * P).unwrap();
    vfs::memfd::seal_fcntl(&f, vfs::memfd::F_ADD_SEALS, vfs::memfd::F_SEAL_SHRINK as u64).unwrap();
    let n = fx.fds.install_flags(f.clone(), 0, false).unwrap() as u32;
    let mut b = vec![0u8; 24];
    b[0..4].copy_from_slice(&n.to_le_bytes());
    b[16..24].copy_from_slice(&(pages * P).to_le_bytes());
    fx.m.b.lock().unwrap()[512..536].copy_from_slice(&b);
    let fd = crate::udmabuf::ioctl_with(crate::udmabuf::UDMABUF_CREATE, BASE + 512, &fx.m, Some(&fx.fds)).unwrap();
    (f, fd as i32)
}

#[test]
fn create_checks_in_linux_order() {
    let fx = Fx::new();
    assert_eq!(fx.create(P, AMDGPU_GEM_DOMAIN_DOORBELL as u64, 0), Err(EINVAL), "6.6 refuses doorbell objects");
    assert_eq!(fx.create(P, GTT, 1 << 30), Err(EINVAL));
    assert_eq!(fx.create(P, GTT, AMDGPU_GEM_CREATE_VRAM_CONTIGUOUS as u64), Err(EINVAL), "not in 6.6's list");
    assert_eq!(fx.create(P, 0x80, 0), Err(EINVAL));
    assert_eq!(fx.create(P, GTT | 1 << 32, 0), Err(EINVAL));
    assert_eq!(fx.create(1, AMDGPU_GEM_DOMAIN_GDS as u64, AV), Err(EINVAL));
    assert_eq!(fx.create(0, GTT, 0), Err(ENOSPC), "no offset for zero pages");
    assert_eq!(fx.create(0, VRAM, CAR), Err(ENOSPC), "after both retries");
    assert_eq!(fx.create(1 << 62, GTT, 0), Err(ENOMEM));
    assert_eq!(fx.create(1 << 62, VRAM, 0), Err(ENOMEM));
    let gtt = fx.gpu.memory().gtt_total;
    assert_eq!(fx.create(gtt, GTT, 0), Err(ENOMEM), "the size must be below GTT's");
    // The 780M has TMZ: encrypted objects are allowed.
    let h = fx.create(P, GTT, AMDGPU_GEM_CREATE_ENCRYPTED as u64).unwrap();
    assert!(h > 0);
    assert_eq!(fx.f(0).drm_file().handle_count(), 1);
}

#[test]
fn create_info_reports_linuxs_shapes() {
    let fx = Fx::new();
    let gds = AMDGPU_GEM_DOMAIN_GDS as u64;
    let oa = AMDGPU_GEM_DOMAIN_OA as u64;
    // (size, align, domains, flags) -> (size, align, domains, flags) as GET_GEM_CREATE_INFO answers.
    let cases = [
        ((1, 0, GTT, 0), (P, 0, GTT, 0)),
        ((P + 1, 1, GTT, USWC), (2 * P, P, GTT, USWC)),
        ((3 * P, 3 * P, GTT, EXPLICIT), (3 * P, 3 * P, GTT, EXPLICIT)),
        ((P, 1 << 16, VRAM, CAR), (P, 1 << 16, VRAM, 0)),
        ((P, P, VRAM | GTT, 0), (P, P, VRAM | GTT, 0)),
        ((P, 0, CPU, 0), (P, 0, CPU, 0)),
        ((P, 0, 0, 0), (P, 0, 0, 0)),
        ((P, 0, GTT, NO_CPU), (P, 0, GTT, NO_CPU)),
        // GDS counts 4-byte units, GWS/OA entries: a page each; no CPU access.
        ((5, 2, gds, 0), (8 * P, 4 * P, gds, NO_CPU)),
        ((3, 1, oa, 0), (3 * P, P, oa, NO_CPU)),
    ];
    for ((s, a, d, f), want) in cases {
        let h = fx.create_in(0, s, a, d, f).unwrap();
        let i = fx.info(0, h).unwrap();
        assert_eq!((i.bo_size, i.alignment, i.domains, i.domain_flags), want, "create {s} {a} {d} {f:#x}");
        fx.close(0, h).unwrap();
    }
    assert_eq!(fx.op(0x7fff_0000, AMDGPU_GEM_OP_GET_GEM_CREATE_INFO, BASE), Err(ENOENT));
    let h = fx.create(P, GTT, 0).unwrap();
    assert_eq!(fx.op(h, AMDGPU_GEM_OP_GET_GEM_CREATE_INFO, 8), Err(EFAULT));
    assert_eq!(fx.op(h, 9, 0), Err(EINVAL));
    let bo = fx.bo(0, h);
    assert_eq!(bo.place(), Some(BoPlace::Gtt));
    assert_eq!(bo.allowed_domains(), GTT as u32);
    let v = fx.create(P, VRAM, 0).unwrap();
    assert_eq!(fx.bo(0, v).allowed_domains(), (VRAM | GTT) as u32, "a VRAM-only object may go to GTT");
    let d = fx.create(P, VRAM, AMDGPU_GEM_CREATE_DISCARDABLE as u64).unwrap();
    assert_eq!(fx.bo(0, d).allowed_domains(), VRAM as u32, "... unless discardable");
    assert_eq!(fx.op(h, AMDGPU_GEM_OP_SET_PLACEMENT, VRAM), Ok(0));
    assert_eq!((bo.preferred_domains(), bo.allowed_domains()), (VRAM as u32, (VRAM | GTT) as u32));
    assert_eq!(fx.op(h, AMDGPU_GEM_OP_SET_PLACEMENT, 0xff), Ok(0));
    assert_eq!(fx.info(0, h).unwrap().domains, 7);
    // A CPU fault remembers CPU access (amdgpu_bo_fault_reserve_notify).
    bo.cpu_touched();
    assert_eq!(fx.info(0, h).unwrap().domain_flags, CAR);
}

/// Any carve-out size: objects count in INFO's usage, a full carve-out sends VRAM objects to GTT (the retry of
/// amdgpu_gem_create_ioctl), and the numbers come back when the objects go.
#[test]
fn vram_for_any_carveout() {
    for carve in [512u64 << 20, 8 << 30] {
        let base = FakeMemory::with_carveout(carve);
        // All of the carve-out but 64 KiB taken by the kernel driver, and a small GTT, so no gigabytes are needed.
        let mem = FakeMemory { kernel_vram_other: carve - base.kernel_vram_pinned - (64 << 10), gtt: base.kernel_gtt_pinned + 16 * P, ..base };
        let fx = Fx::with(FakeGpu::radeon_780m_with(mem));
        assert_eq!(fx.gpu.memory().vram_total, carve);
        assert_eq!(fx.create(carve, VRAM, 0), Err(ENOMEM), "{carve}: not below VRAM's size nor GTT's");
        let a = fx.create(64 << 10, VRAM, 0).unwrap();
        assert_eq!(fx.bo(0, a).place(), Some(BoPlace::Vram));
        assert_eq!(fx.gpu.bo_usage(), (64 << 10, 0));
        assert_eq!(fx.gpu.memory().vram_usage, carve);
        let b = fx.create(P, VRAM, 0).unwrap();
        assert_eq!(fx.bo(0, b).place(), Some(BoPlace::Gtt), "{carve}: full carve-out, the retry adds GTT");
        assert_eq!(fx.info(0, b).unwrap().domains, VRAM | GTT);
        let c = fx.create(P, VRAM | GTT, 0).unwrap();
        assert_eq!(fx.bo(0, c).place(), Some(BoPlace::Gtt));
        assert_eq!(fx.gpu.bo_usage(), (64 << 10, 2 * P));
        assert_eq!(fx.gpu.memory().gtt_usage, base.kernel_gtt_pinned + 2 * P);
        fx.close(0, a).unwrap();
        assert_eq!(fx.gpu.bo_usage(), (0, 2 * P));
        let d = fx.create(P, VRAM, 0).unwrap();
        assert_eq!(fx.bo(0, d).place(), Some(BoPlace::Vram), "room again");
    }
}

#[test]
fn gtt_pool_full_is_enomem() {
    let mem = FakeMemory { gtt: FakeMemory::CAPTURED.kernel_gtt_pinned + 8 * P, ..FakeMemory::CAPTURED };
    let fx = Fx::with(FakeGpu::radeon_780m_with(mem));
    let a = fx.create(8 * P, GTT, 0).unwrap();
    assert_eq!(fx.create(P, GTT, 0), Err(ENOMEM));
    assert!(fx.create(P, CPU, 0).is_ok(), "system objects use no pool");
    fx.close(0, a).unwrap();
    assert!(fx.create(P, GTT, 0).is_ok());
}

#[test]
fn mmap_offsets_and_view_rules() {
    let fx = Fx::new();
    let a = fx.create(2 * P, GTT, 0).unwrap();
    let b = fx.create(P, VRAM, 0).unwrap();
    let c = fx.create(P, GTT, NO_CPU).unwrap();
    let (r, oa) = fx.offset(0, a);
    assert_eq!(r, Ok(0));
    let (_, ob) = fx.offset(0, b);
    assert!(oa >= MMAP_START_PG * P && ob >= MMAP_START_PG * P && oa % P == 0 && oa != ob);
    assert_eq!(fx.offset(0, c), (Err(EPERM), 0), "no CPU access; the answer is zeroed");
    assert_eq!(fx.offset(0, 0x7fff_0000), (Err(ENOENT), 0));
    // Offsets are handed out upwards first; freed ranges come back only once the end is reached.
    fx.close(0, a).unwrap();
    let a2 = fx.create(2 * P, GTT, 0).unwrap();
    assert!(fx.offset(0, a2).1 > ob);
    // The view: host objects have no pages, so a udmabuf import (page-cache frames) stands in.
    let (_m, ub) = udmabuf(&fx, 2);
    let h = fx.to_handle(0, ub).unwrap();
    let (_, ou) = fx.offset(0, h);
    let gem = fx.gpu.shared().gem.clone();
    let view = |k: usize, off: u64, pages: u64| crate::gem_map::view_answers(fx.f(k).drm_file(), &gem, off / P, pages);
    assert_eq!(view(0, ou, 2), (Ok(()), Some((0, 0))));
    assert_eq!(view(0, ou, 3).0, Err(EINVAL), "past the object");
    assert_eq!(view(0, ou + P, 1), (Err(EINVAL), Some((0, 1))), "not at its start; a fault inside is fine");
    assert_eq!(view(1, ou, 1).0, Err(EACCES), "another file's object");
    assert_eq!(view(0, 1 << 46, 1).0, Err(EINVAL));
    assert_eq!(view(1, 1 << 46, 1).0, Err(EINVAL), "a file without objects: EINVAL as drm_gem_mmap, not ENODEV");
    assert_eq!(fx.bo(0, h).flags() & CAR, CAR, "the fault remembered CPU access");
    // CR-DR6-1's per-object maps answer the same.
    assert_eq!(fx.f(0).object_map(ou / P + 1, 1).err(), Some(EINVAL));
    assert_eq!(fx.f(1).object_map(ou / P, 1).err(), Some(EACCES));
    assert_eq!(fx.f(0).object_map(fx.bo(0, c).mmap_offset() / P, 1).err(), Some(EPERM));
    assert!(fx.f(0).object_map(ou / P, 2).is_ok());
    // The file's view; without objects it is a refusing view of one page, which host tests have no frame for
    // (dr.gem.view checks it in the kernel; until CR-DR6-1).
    assert!(crate::gem_map::file_map(fx.f(0).drm_file(), &gem).is_some());
    assert!(crate::gem_map::file_map(fx.f(1).drm_file(), &gem).is_none());
}

#[test]
fn metadata_and_its_echo() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    let mut a = drm_amdgpu_gem_metadata::ZERO;
    a.handle = 0x7fff_0000;
    a.op = AMDGPU_GEM_METADATA_OP_GET_METADATA;
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_GEM_METADATA), Err(ENOENT));
    let get = || {
        let mut a = drm_amdgpu_gem_metadata::ZERO;
        a.handle = h;
        a.op = AMDGPU_GEM_METADATA_OP_GET_METADATA;
        a.data.data = [0x5a5a_5a5a; 64];
        a.data.flags = 0x5a;
        fx.m.put(0, &a);
        assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_GEM_METADATA), Ok(0));
        fx.m.take::<drm_amdgpu_gem_metadata>(0).data
    };
    let d = get();
    assert_eq!((d.tiling_info, d.data_size_bytes, d.flags, d.data[0]), (0, 0, 0, 0x5a5a_5a5a));
    let set = |size: u32, flags: u64, tiling: u64| {
        let mut a = drm_amdgpu_gem_metadata::ZERO;
        a.handle = h;
        a.op = AMDGPU_GEM_METADATA_OP_SET_METADATA;
        a.data.flags = flags;
        a.data.tiling_info = tiling;
        a.data.data_size_bytes = size;
        a.data.data[0] = 0x1122_3344;
        a.data.data[1] = 0x5566_7788;
        fx.m.put(0, &a);
        fx.io(0, DRM_IOCTL_AMDGPU_GEM_METADATA)
    };
    assert_eq!(set(257, 5, 1), Err(EINVAL));
    assert_eq!(set(6, 5, 0x1234 << 40 | 27), Ok(0));
    let d = get();
    assert_eq!((d.tiling_info, d.data_size_bytes, d.flags), (0x1234 << 40 | 27, 6, 5));
    assert_eq!((d.data[0], d.data[1], d.data[2]), (0x1122_3344, 0x5a5a_7788, 0x5a5a_5a5a), "bytes past the metadata echo");
    assert_eq!(set(0, 9, 0), Ok(0));
    let d = get();
    assert_eq!((d.data_size_bytes, d.flags, d.tiling_info), (0, 5, 0), "size 0 drops the data, keeps the flags");
    let mut a = drm_amdgpu_gem_metadata::ZERO;
    a.handle = h;
    a.op = 7;
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_GEM_METADATA), Ok(0), "an unknown op is 0 in Linux");
}

#[test]
fn userptr_refused_cleanly() {
    let fx = Fx::new();
    let u = |addr: u64, size: u64, flags: u32| {
        fx.m.put(0, &drm_amdgpu_gem_userptr { addr, size, flags, handle: 0 });
        fx.io(0, DRM_IOCTL_AMDGPU_GEM_USERPTR)
    };
    assert_eq!(u(0x10001, P, 1), Err(EINVAL));
    assert_eq!(u(0x10000, 100, 1), Err(EINVAL));
    assert_eq!(u(0x10000, P, 0x10), Err(EINVAL));
    assert_eq!(u(0x10000, P, 0), Err(EACCES));
    assert_eq!(u(0x10000, 0, 1), Err(ENOSPC));
    assert_eq!(u(0x10000, P, AMDGPU_GEM_USERPTR_READONLY), Err(ENODEV));
    assert_eq!(u(0x10000, P, AMDGPU_GEM_USERPTR_REGISTER | AMDGPU_GEM_USERPTR_VALIDATE), Err(ENODEV));
    assert_eq!(fx.f(0).drm_file().handle_count(), 0);
}

#[test]
fn wait_idle_on_ring_fences() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    let e = fx.create(P, GTT, EXPLICIT).unwrap();
    assert_eq!(fx.wait(h, 0), Ok(0));
    assert_eq!(fx.wait(0x7fff_0000, 0), Err(ENOENT));
    let bo = fx.bo(0, h);
    let f = fx.gpu.ring().emit();
    bo.fence(&f, false).unwrap();
    fx.bo(0, e).fence(&f, false).unwrap();
    assert_eq!(fx.wait(h, 0), Ok(1), "busy at once");
    assert_eq!(fx.wait(h, 1), Ok(1), "a past deadline");
    assert_eq!(bo.implicit_deps(false).len(), 1, "a reader waits for the writer");
    assert!(fx.bo(0, e).implicit_deps(true).is_empty(), "explicit sync waits for nothing implicit");
    let r = fx.gpu.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(30));
        r.ring().signal_all()
    });
    assert_eq!(fx.wait(h, u64::MAX), Ok(0), "no deadline: until the fence signals");
    assert_eq!(t.join().unwrap(), 1);
    assert!(bo.implicit_deps(true).is_empty());
    // A kill ends a wait.
    bo.fence(&fx.gpu.ring().emit(), true).unwrap();
    fx.m.kill.store(true, Ordering::SeqCst);
    assert_eq!(fx.wait(h, u64::MAX), Err(EINTR));
    fx.m.kill.store(false, Ordering::SeqCst);
    fx.gpu.ring().signal_all();
    assert_eq!(fx.wait(h, 0), Ok(0));
}

#[test]
fn prime_export_import_one_buffer() {
    let fx = Fx::new();
    let h = fx.create(3 * P, GTT, 0).unwrap();
    assert_eq!(fx.to_fd(0, h, 1), Err(EINVAL));
    assert_eq!(fx.to_fd(0, 0x7fff_0000, DRM_CLOEXEC | DRM_RDWR), Err(ENOENT));
    let b1 = fx.to_fd(0, h, DRM_CLOEXEC | DRM_RDWR).unwrap();
    let b2 = fx.to_fd(0, h, DRM_RDWR).unwrap();
    assert_eq!(fx.fds.cloexec(b1 as usize), Ok(true));
    assert_eq!(fx.fds.cloexec(b2 as usize), Ok(false));
    let (buf, bo) = (fx.buf(b1), fx.bo(0, h));
    assert!(Arc::ptr_eq(&buf, &fx.buf(b2)), "one buffer");
    assert!(Arc::ptr_eq(&fx.fds.get(b1 as usize).unwrap(), &fx.fds.get(b2 as usize).unwrap()), "one open file");
    assert!(Arc::ptr_eq(buf.resv(), bo.resv()), "the object's reservation object");
    assert_eq!(buf.size(), 3 * P);
    assert_eq!(buf.exporter().name(), "drm");
    assert_eq!(buf.lseek(0, vfs::abi::SEEK_END), Some(Ok(3 * P)));
    // Implicit sync: a job's write fence on the object is the dma-buf's (poll waits for it).
    let f = fx.gpu.ring().emit();
    bo.fence(&f, false).unwrap();
    assert_eq!(buf.events().0 & vfs::poll::Events::IN, 0);
    fx.gpu.ring().signal_all();
    assert_ne!(buf.events().0 & vfs::poll::Events::IN, 0);
    // Writes through the object show in the buffer.
    assert!(bo.write(2 * P + 5, b"prime"));
    let mut got = [0u8; 5];
    assert!(buf.exporter().read(2 * P + 5, &mut got));
    assert_eq!(&got, b"prime");
    // Back: the same handle; in the second file the same object.
    assert_eq!(fx.to_handle(0, b1), Ok(h));
    assert_eq!(fx.to_handle(0, b2), Ok(h));
    let o = fx.to_handle(1, b1).unwrap();
    assert!(Arc::ptr_eq(&fx.bo(1, o), &bo));
    assert_eq!(fx.to_handle(1, b2), Ok(o));
    assert_eq!(bo.handle_count(), 2);
    assert_eq!(fx.to_handle(0, -1), Err(EBADF));
    let (mf, _) = udmabuf(&fx, 1);
    let plain = fx.fds.install_flags(mf, 0, false).unwrap() as i32;
    assert_eq!(fx.to_handle(0, plain), Err(EINVAL), "a memfd is no dma-buf");
    // Every handle gone: the buffer lives on in its fds; importing it gives the object back.
    let wb = Arc::downgrade(&buf);
    let wo: Weak<Bo> = Arc::downgrade(&bo);
    drop((buf, bo));
    fx.close(0, h).unwrap();
    fx.close(1, o).unwrap();
    assert!(wo.upgrade().is_some(), "the buffer holds the object");
    let again = fx.to_handle(0, b1).unwrap();
    let mut back = [0u8; 5];
    assert!(fx.bo(0, again).read(2 * P + 5, &mut back));
    assert_eq!(&back, b"prime");
    fx.close(0, again).unwrap();
    fx.fds.close(b1 as usize).unwrap();
    fx.fds.close(b2 as usize).unwrap();
    assert!(wb.upgrade().is_none() && wo.upgrade().is_none(), "no cycle between object and buffer");
}

#[test]
fn prime_modes_and_refusals() {
    let fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    let r = fx.to_fd(0, h, DRM_CLOEXEC).unwrap();
    assert!(!fx.fds.get(r as usize).unwrap().can_write(), "read-only without DRM_RDWR");
    let w = fx.to_fd(0, h, DRM_CLOEXEC | DRM_RDWR).unwrap();
    assert!(!fx.fds.get(w as usize).unwrap().can_write(), "the first export's file is shared");
    let av = fx.create(P, GTT, AV).unwrap();
    assert_eq!(fx.to_fd(0, av, DRM_RDWR), Err(EPERM));
    assert!(Arc::ptr_eq(fx.bo(0, av).resv(), &fx.f(0).drm_file().gem.vm_resv), "always-valid objects share the VM's");
    let n = fx.create(P, GTT, NO_CPU).unwrap();
    let nb = fx.to_fd(0, n, DRM_RDWR).unwrap();
    assert_eq!(fx.buf(nb).exporter().mmap_check(true), Err(EPERM));
    // A foreign buffer: a udmabuf.
    let (_m, ub) = udmabuf(&fx, 2);
    let i = fx.to_handle(0, ub).unwrap();
    assert_eq!(fx.to_handle(0, ub), Ok(i));
    let info = fx.info(0, i).unwrap();
    assert_eq!((info.bo_size, info.alignment, info.domains, info.domain_flags), (2 * P, P, GTT, 0));
    let ibo = fx.bo(0, i);
    assert!(ibo.is_import() && Arc::ptr_eq(ibo.resv(), fx.buf(ub).resv()));
    assert_eq!(fx.op(i, AMDGPU_GEM_OP_SET_PLACEMENT, VRAM), Err(EINVAL));
    assert_eq!(fx.op(i, AMDGPU_GEM_OP_SET_PLACEMENT, GTT), Ok(0));
    let re = fx.to_fd(0, i, DRM_RDWR).unwrap();
    assert!(Arc::ptr_eq(&fx.buf(re), &fx.buf(ub)), "re-export gives the udmabuf itself");
    assert_eq!(fx.gpu.bo_usage(), (0, 3 * P), "an import uses no pool");
}

#[test]
fn last_close_releases_everything() {
    let mut fx = Fx::new();
    let h = fx.create(P, GTT, 0).unwrap();
    let v = fx.create(P, VRAM, 0).unwrap();
    let b = fx.to_fd(0, h, DRM_RDWR).unwrap();
    fx.to_handle(1, b).unwrap();
    let wv = Arc::downgrade(&fx.bo(0, v));
    let wh = Arc::downgrade(&fx.bo(0, h));
    assert_eq!(fx.gpu.bo_usage(), (P, P));
    fx.files[0] = None;
    assert!(wv.upgrade().is_none(), "the VRAM object went with the file");
    assert_eq!(fx.gpu.bo_usage(), (0, P));
    fx.files[1] = None;
    assert!(wh.upgrade().is_some(), "the fd keeps the other");
    fx.fds.close(b as usize).unwrap();
    assert!(wh.upgrade().is_none());
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
}

#[test]
fn concurrent_creates_and_closes() {
    let fx = Arc::new(Fx::new());
    let mut ts = Vec::new();
    for k in 0..4u64 {
        let fx = fx.clone();
        ts.push(thread::spawn(move || {
            let file = fx.f((k % 2) as usize);
            for i in 0..200u64 {
                let bo = gem::create_bo(&*fx.gpu, &file.drm_file().gem, P * (1 + i % 3), 0, GTT as u32, 0, None).unwrap();
                let h = file.drm_file().handle_create(bo.clone()).unwrap();
                assert!(bo.write(0, &[k as u8]));
                file.drm_file().handle_delete(h).unwrap();
            }
        }));
    }
    for t in ts {
        t.join().unwrap();
    }
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
    assert_eq!(fx.f(0).drm_file().handle_count() + fx.f(1).drm_file().handle_count(), 0);
}

const GDS: u64 = AMDGPU_GEM_DOMAIN_GDS as u64;
/// GDS entries whose objects take half of the offset space (`DRM_FILE_PAGE_OFFSET_SIZE`: 0xfffff * 256 pages).
const HALF: u64 = 1 << 27;

#[test]
fn offset_space_bounded_and_reused() {
    let fx = Fx::new();
    // A GDS object has no pages: only the offset space bounds it, and a refusal changes nothing.
    assert_eq!(fx.create(1 << 40, GDS, 0), Err(ENOSPC));
    assert_eq!(fx.create(u64::MAX >> 13, GDS, 0), Err(ENOSPC));
    let a = fx.create(P, GTT, 0).unwrap();
    let (_, oa) = fx.offset(0, a);
    assert!(oa >= MMAP_START_PG * P);
    let g1 = fx.create(HALF, GDS, 0).unwrap();
    assert_eq!(fx.create(HALF, GDS, 0), Err(ENOSPC), "the space is 1 TiB of offsets, as Linux's");
    let b = fx.create(P, GTT, 0).unwrap();
    // The bump has passed the second half's start; freed ranges are found again.
    fx.close(0, g1).unwrap();
    let g2 = fx.create(HALF, GDS, 0).unwrap();
    assert_eq!(fx.create(HALF, GDS, 0), Err(ENOSPC));
    fx.close(0, g2).unwrap();
    for h in [a, b] {
        fx.close(0, h).unwrap();
    }
    assert!(fx.create(P, GTT, 0).is_ok(), "after every refusal other clients still get objects");
}

impl Fx {
    /// File `k` closed and opened again as a fresh render file.
    pub(crate) fn reopen(&mut self, k: usize) {
        self.files[k] = None;
        self.files[k] = Some(open_device(self.dev.clone(), MinorKind::Render, false).unwrap());
    }
    pub(crate) fn open_primary(&mut self, authenticated: bool) -> usize {
        self.files.push(Some(open_device(self.dev.clone(), MinorKind::Primary, authenticated).unwrap()));
        self.files.len() - 1
    }
    pub(crate) fn flink(&self, k: usize, h: u32) -> Result<u32, vfs::Errno> {
        self.m.put(0, &drm_gem_flink { handle: h, name: 0 });
        self.io(k, DRM_IOCTL_GEM_FLINK)?;
        Ok(self.m.take::<drm_gem_flink>(0).name)
    }
    pub(crate) fn open_name(&self, k: usize, name: u32) -> Result<(u32, u64), vfs::Errno> {
        self.m.put(0, &drm_gem_open { name, handle: 0, size: 0 });
        self.io(k, DRM_IOCTL_GEM_OPEN)?;
        let a = self.m.take::<drm_gem_open>(0);
        Ok((a.handle, a.size))
    }
}

#[test]
fn flink_and_open_names() {
    let mut fx = Fx::new();
    let (p, q, anon) = (fx.open_primary(true), fx.open_primary(true), fx.open_primary(false));
    let h = fx.create_in(p, 2 * P, 0, GTT, 0).unwrap();
    // DRM_AUTH without RENDER_ALLOW (drm_ioctl.c:603-604): EACCES on a render node and unauthenticated.
    let r = fx.create(P, GTT, 0).unwrap();
    assert_eq!(fx.flink(0, r), Err(EACCES));
    assert_eq!(fx.open_name(0, 1), Err(EACCES));
    assert_eq!(fx.flink(anon, h), Err(EACCES));
    // drm_gem_flink_ioctl: ENOENT for an unknown handle; one name per object, from 1.
    assert_eq!(fx.flink(p, 0x7fff_0000), Err(ENOENT));
    let n = fx.flink(p, h).unwrap();
    assert!(n >= 1);
    assert_eq!(fx.flink(p, h), Ok(n));
    // drm_gem_open_ioctl: the same object in another file, with its size.
    let (h2, size) = fx.open_name(q, n).unwrap();
    assert_eq!(size, 2 * P);
    assert!(Arc::ptr_eq(&fx.bo(p, h), &fx.bo(q, h2)));
    for bad in [0, n + 100, 0x8000_0000 | n, u32::MAX] {
        assert_eq!(fx.open_name(q, bad), Err(ENOENT), "name {bad:#x}");
    }
    // The name lives while some file has a handle.
    fx.close(p, h).unwrap();
    let (h3, _) = fx.open_name(p, n).unwrap();
    fx.close(p, h3).unwrap();
    fx.close(q, h2).unwrap();
    assert_eq!(fx.open_name(q, n), Err(ENOENT), "the last handle took the name");
    // An always-valid object stays in its own file's VM (amdgpu_gem_object_open): EPERM, and nothing counted.
    let av = fx.create_in(p, P, 0, GTT, AV).unwrap();
    let na = fx.flink(p, av).unwrap();
    assert_eq!(fx.open_name(q, na), Err(EPERM));
    assert_eq!(fx.bo(p, av).st.lock().handles, 1, "the refused open gave its count back");
    let (av2, _) = fx.open_name(p, na).unwrap();
    assert_ne!(av2, av, "the own file gets a second handle (drm_gem_handle_create_tail)");
    assert_eq!(fx.bo(p, av).st.lock().handles, 2);
    fx.close(p, av).unwrap();
    fx.close(p, av2).unwrap();
    assert_eq!(fx.open_name(p, na), Err(ENOENT));
}
