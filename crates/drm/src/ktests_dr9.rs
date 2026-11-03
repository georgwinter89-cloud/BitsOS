//! DR-9 in-kernel tests (Safe, rerunnable; each makes a fake device of its own, so the boot device and no
//! hardware is touched): a CS by BO list handle, FENCE_TO_HANDLE into a syncobj and a sync_file, an UNMAP that
//! waits for the job queued before it, the GPUVM fault query after a job's fault, a userptr object over a fake
//! address space (the job writes the program's memory, an unmap makes the next CS EFAULT), the ioctl fuzz pass,
//! and the ABI program abi-drm-list against CachyOS's transcript. Every test checks that no frame, object or
//! dma-buf is left.

use crate::fake::{FakeGpu, FakeMemory};
use crate::fence;
use crate::file::MinorKind;
use crate::ktests::{with_dev, Dev, KMem, BASE, KERNEL};
use crate::ktests_dr6::{create, put, take, Balance};
use crate::ktests_dr8::{bytes, cs_with, ctx_alloc, io, ib_chunk, own_device, own_device_with, program, setup, syncobj, wait_cs, RWX};
use crate::node::{open_device, OpenDrm};
use crate::uapi::*;
use crate::userptr::FakeUserPages;
use crate::wire::Wire;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::pm4;
use ktest::{kassert, ktest, TestResult};

const UADDR: u64 = 0x7f00_0000_0000;
const UVA: u64 = 0x2_0000_0000;
/// The ktests' fake address spaces (the registry is the kernel's: ids no program has).
const MM_K: u64 = u64::MAX - 9;
const MM_FUZZ: u64 = u64::MAX - 10;

fn open(dev: &Arc<crate::device::Device>) -> Result<Arc<OpenDrm>, String> {
    open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))
}

fn bo_list(f: &OpenDrm, m: &KMem, op: u32, handle: u32, hs: &[u32]) -> Result<u32, vfs::Errno> {
    let list: Vec<u8> = hs.iter().flat_map(|&h| bytes(&drm_amdgpu_bo_list_entry { bo_handle: h, bo_priority: 0 })).collect();
    m.set(6144, &list);
    let mut a = drm_amdgpu_bo_list::ZERO;
    a.set_in(&drm_amdgpu_bo_list_in { operation: op, list_handle: handle, bo_number: hs.len() as u32, bo_info_size: 8, bo_info_ptr: BASE + 6144 });
    put(m, 0, &a);
    f.ioctl_as(DRM_IOCTL_AMDGPU_BO_LIST, BASE, m, KERNEL)?;
    Ok(m.u64(0) as u32)
}

fn sem(id: u32, h: u32) -> (u32, Vec<u8>) {
    (id, h.to_le_bytes().to_vec())
}

fn gem_va(f: &OpenDrm, m: &KMem, h: u32, op: u32, va: u64, size: u64) -> Result<(), String> {
    put(m, 0, &drm_amdgpu_gem_va { handle: h, operation: op, flags: RWX, va_address: va, offset_in_bo: 0, map_size: size, ..drm_amdgpu_gem_va::ZERO });
    io(f, m, DRM_IOCTL_AMDGPU_GEM_VA, "GEM_VA")
}

fn write_to(va: u64, v: u32) -> Vec<u32> {
    let mut p = Vec::new();
    let _ = pm4::write_data(&mut p, pm4::dst_sel::MEM, va, 0, &[v]);
    p
}

fn bo_list_cs() -> TestResult {
    own_device(|dev, _gpu| {
        let f = open(dev)?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        let p = program(0x0bad_cafe, 5);
        let h = setup(&f, &m, &p)?;
        let x = create(&f, &m, 4096, AMDGPU_GEM_DOMAIN_GTT, 0)?;
        let l = bo_list(&f, &m, AMDGPU_BO_LIST_OP_CREATE, 0, &[h, x]).map_err(|e| format!("BO_LIST CREATE: errno {e}"))?;
        kassert!(l == 1, "first list handle {l}");
        kassert!(cs_with(&f, &m, c, l, &[ib_chunk(p.len())]) == Ok(1), "CS by list handle");
        kassert!(wait_cs(&f, &m, c, 1) == Ok(0), "the job ran");
        let mut b = [0u8; 4];
        kassert!(f.drm_file().bo(h).ok_or("no object")?.read(0x800, &mut b) && u32::from_le_bytes(b) == 0x0bad_cafe, "job bytes {b:x?}");
        kassert!(bo_list(&f, &m, AMDGPU_BO_LIST_OP_UPDATE, l, &[h]) == Ok(l), "UPDATE");
        kassert!(bo_list(&f, &m, AMDGPU_BO_LIST_OP_UPDATE, l + 5, &[h]) == Err(linux::errno::ENOENT), "UPDATE of an unknown list");
        kassert!(bo_list(&f, &m, AMDGPU_BO_LIST_OP_DESTROY, l, &[]) == Ok(0), "DESTROY");
        kassert!(cs_with(&f, &m, c, l, &[ib_chunk(p.len())]) == Err(linux::errno::ENOENT), "CS by a destroyed list");
        Ok(())
    })
}
ktest!("dr.bo_list.cs", Safe, bo_list_cs);

fn fence_to_handle() -> TestResult {
    own_device(|dev, gpu| {
        let f = open(dev)?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        let p = program(1, 1);
        setup(&f, &m, &p)?;
        let sin = syncobj(&f, &m)?;
        let gate = gpu.ring().emit();
        f.drm_file().syncobj(sin).ok_or("no syncobj")?.replace_fence(Some(gate));
        kassert!(cs_with(&f, &m, c, 0, &[ib_chunk(p.len()), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, sin)]) == Ok(1), "CS");
        let fds = Arc::new(vfs::fd::FdTable::new());
        let mut got = Vec::new();
        for what in [AMDGPU_FENCE_TO_HANDLE_GET_SYNCOBJ, AMDGPU_FENCE_TO_HANDLE_GET_SYNC_FILE_FD] {
            let mut a = drm_amdgpu_fence_to_handle::ZERO;
            a.set_in(&drm_amdgpu_fence_to_handle__in { fence: drm_amdgpu_fence { ctx_id: c, ip_type: 0, ip_instance: 0, ring: 0, seq_no: 1 }, what, pad: 0 });
            put(&m, 0, &a);
            f.ioctl_with(DRM_IOCTL_AMDGPU_FENCE_TO_HANDLE, BASE, &m, KERNEL, Some(fds.clone())).map_err(|e| format!("FENCE_TO_HANDLE {what}: errno {e}"))?;
            got.push(m.u64(0) as u32);
        }
        let so = f.drm_file().syncobj(got[0]).and_then(|o| o.fence()).ok_or("no syncobj fence")?;
        let file = fds.get(got[1] as usize).map_err(|e| format!("sync_file fd: errno {e}"))?.file.clone();
        let sf = crate::sync_file::as_sync_file(&file).ok_or("not a sync_file")?;
        kassert!(!fence::is_signaled(&so) && !fence::is_signaled(sf.fence()), "the job waits for its gate");
        gpu.ring().signal_all();
        kassert!(fence::status(&so) == 1 && fence::status(sf.fence()) == 1, "both signal with the job");
        drop(file);
        drop(fds);
        Ok(())
    })
}
ktest!("dr.fence.to_handle", Safe, fence_to_handle);

fn unmap_waits() -> TestResult {
    own_device(|dev, gpu| {
        let f = open(dev)?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        let p = program(0x55, 2);
        let h = setup(&f, &m, &p)?;
        let sin = syncobj(&f, &m)?;
        let gate = gpu.ring().emit();
        f.drm_file().syncobj(sin).ok_or("no syncobj")?.replace_fence(Some(gate));
        kassert!(cs_with(&f, &m, c, 0, &[ib_chunk(p.len()), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, sin)]) == Ok(1), "CS");
        gem_va(&f, &m, h, AMDGPU_VA_OP_UNMAP, crate::ktests_dr8::V, 8192)?;
        let vm = f.drm_file().vm.backend_vm().ok_or("no VM")?;
        let fake = vm.as_fake().ok_or("not the fake VM")?;
        kassert!(fake.deferred() == 1 && fake.translate(crate::ktests_dr8::V).is_some(), "the Clear waits for the job");
        gpu.ring().signal_all();
        kassert!(wait_cs(&f, &m, c, 1) == Ok(0), "the job ran on its mapping");
        kassert!(fake.deferred() == 0 && fake.translate(crate::ktests_dr8::V).is_none(), "then the Clear ran");
        Ok(())
    })
}
ktest!("dr.vm.unmap_waits", Safe, unmap_waits);

fn fault_query() -> TestResult {
    own_device(|dev, _gpu| {
        let f = open(dev)?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        let p = write_to(0x6_0000_2000, 9);
        setup(&f, &m, &p)?;
        kassert!(cs_with(&f, &m, c, 0, &[ib_chunk(p.len())]) == Ok(1), "CS");
        kassert!(wait_cs(&f, &m, c, 1) == Err(linux::errno::ETIME), "the job faults");
        let mut req = drm_amdgpu_info::ZERO;
        req.return_pointer = BASE + 4096;
        req.return_size = 16;
        req.query = AMDGPU_INFO_GPUVM_FAULT;
        put(&m, 0, &req);
        io(&f, &m, DRM_IOCTL_AMDGPU_INFO, "INFO GPUVM_FAULT")?;
        let r = take::<drm_amdgpu_info_gpuvm_fault>(&m, 4096).ok_or("decode")?;
        kassert!(r.addr == 0x6_0000_2000 && r.status & 0x100 != 0 && (r.status >> 9) & 0x1ff == 6 && r.vmhub == AMDGPU_VMHUB_TYPE_GFX, "fault {r:x?}");
        Ok(())
    })
}
ktest!("dr.vm.fault_query", Safe, fault_query);

fn userptr() -> TestResult {
    own_device(|dev, gpu| {
        // Without a page source (none in the kernel until CR-DR9-2): ENODEV after the argument checks.
        if !crate::userptr::source_set() {
            let f = open(dev)?;
            let m = KMem::new(4096);
            put(&m, 0, &drm_amdgpu_gem_userptr { addr: UADDR, size: 4096, flags: AMDGPU_GEM_USERPTR_REGISTER, handle: 0 });
            let r = f.ioctl_with(DRM_IOCTL_AMDGPU_GEM_USERPTR, BASE, &m, KERNEL, None);
            kassert!(r == Err(linux::errno::ENODEV), "GEM_USERPTR without a page source: {r:?}");
        }
        let sp = FakeUserPages::new(MM_K);
        sp.map(MM_K, UADDR, 1, true, false);
        gpu_shared(gpu).gem.set_user_pages(Some(sp.clone()));
        let f = open(dev)?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        put(&m, 0, &drm_amdgpu_gem_userptr { addr: UADDR, size: 4096, flags: AMDGPU_GEM_USERPTR_REGISTER | AMDGPU_GEM_USERPTR_VALIDATE, handle: 0 });
        io(&f, &m, DRM_IOCTL_AMDGPU_GEM_USERPTR, "GEM_USERPTR")?;
        let u = take::<drm_amdgpu_gem_userptr>(&m, 0).ok_or("decode")?.handle;
        gem_va(&f, &m, u, AMDGPU_VA_OP_MAP, UVA, 4096)?;
        let p = write_to(UVA + 0x20, 0x600d_f00d);
        let h = setup(&f, &m, &p)?;
        let l = bo_list(&f, &m, AMDGPU_BO_LIST_OP_CREATE, 0, &[h, u]).map_err(|e| format!("BO_LIST: errno {e}"))?;
        kassert!(cs_with(&f, &m, c, l, &[ib_chunk(p.len())]) == Ok(1), "CS");
        kassert!(wait_cs(&f, &m, c, 1) == Ok(0), "the job ran");
        let mut b = [0u8; 4];
        kassert!(sp.read(MM_K, UADDR + 0x20, &mut b) && u32::from_le_bytes(b) == 0x600d_f00d, "the program's memory {b:x?}");
        kassert!(sp.unmap(MM_K, UADDR, 1) == 1, "the object heard of the unmap");
        kassert!(cs_with(&f, &m, c, l, &[ib_chunk(p.len())]) == Err(linux::errno::EFAULT), "CS after the unmap");
        drop(f);
        gpu_shared(gpu).gem.set_user_pages(None);
        kassert!(crate::userptr::registered_in(MM_K) == 0, "the object left the registry");
        Ok(())
    })
}
ktest!("dr.userptr.fake", Safe, userptr);

fn gpu_shared(g: &FakeGpu) -> &crate::gpu::GpuShared {
    crate::gpu::GpuDevice::shared(g)
}

/// The test memory for the fuzz: every wait in it ends at once (EINTR), so random timeouts cannot hold it.
struct FuzzMem(KMem);

impl vfs::sys::UserMem for FuzzMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        self.0.read(addr, buf)
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        self.0.write(addr, data)
    }
    fn read_cstr(&self, addr: u64, max: usize) -> Result<Vec<u8>, vfs::Errno> {
        self.0.read_cstr(addr, max)
    }
    fn interrupted(&self) -> bool {
        true
    }
}

/// xorshift64*, fixed seed: a failing call can be found again by its iteration.
fn rng(s: &mut u64) -> u64 {
    *s ^= *s >> 12;
    *s ^= *s << 25;
    *s ^= *s >> 27;
    s.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

/// Every DRM core and amdgpu number with random directions, sizes, pointers (the test memory, its end, NULL, a
/// kernel address, anything) and contents: no panic, every answer a Linux errno or success.
fn fuzz() -> TestResult {
    let small = FakeMemory { carveout: 32 << 20, visible: 32 << 20, gtt: 32 << 20, ..FakeMemory::CAPTURED };
    own_device_with(FakeGpu::radeon_780m_with(small), |dev, gpu| {
        let sp = FakeUserPages::new(MM_FUZZ);
        sp.map(MM_FUZZ, BASE, 2, true, false);
        gpu_shared(gpu).gem.set_user_pages(Some(sp.clone()));
        let files = [open(dev)?, open(dev)?];
        let m = FuzzMem(KMem::new(8192));
        let fds = Arc::new(vfs::fd::FdTable::new());
        let mut sizes: Vec<u32> = vec![8, 16, 24, 32, 40, 64];
        sizes.extend(AMDGPU_IOCTLS.iter().chain(DRM_IOCTLS).map(|(_, c)| crate::ioc::size(*c)));
        let mut s = 0x2545_f491_9e37_79b9u64;
        let mut calls = 0u64;
        for _round in 0..32 {
            for nr in 0u32..0x100 {
                let size = match rng(&mut s) % 4 {
                    0 => sizes[(rng(&mut s) % sizes.len() as u64) as usize],
                    1 => (rng(&mut s) % 0x80) as u32,
                    2 => (rng(&mut s) % 0x4000) as u32,
                    _ => 0,
                };
                let cmd = (((rng(&mut s) % 4) as u32) << 30) | (size << 16) | (0x64 << 8) | nr;
                let arg = match rng(&mut s) % 6 {
                    0 => 0,
                    1 => BASE + 8192 - rng(&mut s) % 64,
                    2 => 0xffff_8000_0000_0000 + rng(&mut s) % (1 << 20),
                    3 => rng(&mut s),
                    _ => BASE + (rng(&mut s) % 64) * 8,
                };
                let mut b = vec![0u8; 8192];
                for w in b.chunks_mut(8) {
                    let v = match rng(&mut s) % 5 {
                        0 => rng(&mut s),
                        1 => rng(&mut s) % 8,
                        2 => BASE + rng(&mut s) % 8192,
                        3 => u64::MAX,
                        _ => 0,
                    };
                    w.copy_from_slice(&v.to_le_bytes());
                }
                m.0.set(0, &b);
                let f = &files[(rng(&mut s) % 2) as usize];
                let r = f.ioctl_with(cmd, arg, &m, KERNEL, Some(fds.clone()));
                calls += 1;
                if let Err(e) = r {
                    kassert!((1..=133).contains(&e), "call {calls} cmd {cmd:#x} arg {arg:#x}: errno {e}");
                }
            }
        }
        drop(files);
        drop(fds);
        gpu.ring().signal_all();
        gpu_shared(gpu).gem.set_user_pages(None);
        kassert!(gpu.cs_stats().2 == 0, "nothing left queued: {:?}", gpu.cs_stats());
        Ok(())
    })
}
ktest!("dr.fuzz.ioctls", Safe, fuzz);

const PROG: &str = "/bin/abi-drm-list";
const EXPECTED: &str = include_str!("../../../host/tests/dr/fixtures/abi-drm-list.txt");

/// The ABI program on V2: every line as on CachyOS.
fn abi_program() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::skip("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::skip("not the fake GPU");
        }
        let before = Balance::now();
        let out = proc::stdio::Capture::new(1 << 16);
        let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
        let argv = ["abi-drm-list"];
        let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
        o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
        o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
        let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {PROG}: {e}"))?;
        let ended = p.wait(Some(60_000_000_000)).is_some();
        if !ended {
            p.kill(9);
            p.wait(Some(5_000_000_000));
        }
        let text = String::from_utf8_lossy(&out.contents()).into_owned();
        kassert!(ended && p.status() == Some(proc::ExitStatus::Exited(0)), "{PROG} ended {:?}: {text}", p.status());
        let diff: Vec<(&str, &str)> = text.lines().zip(EXPECTED.lines()).filter(|(a, b)| a != b).collect();
        kassert!(diff.is_empty() && text.lines().count() == EXPECTED.lines().count(), "{PROG} differs from Linux: {diff:?}");
        drop(p);
        before.check()
    })
}
ktest!("dr.list.program", Safe, abi_program);
