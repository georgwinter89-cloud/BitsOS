//! DR-8 in-kernel tests (Safe, rerunnable; the job tests make a fake device of their own, so the boot device's
//! reset counter and rings never move): a CS whose IB runs on the fake's PM4 interpreter through real GFX 11
//! tables and frames and writes memory byte for byte, the user fence, WAIT_CS and WAIT_FENCES; a hang (ETIME,
//! guilty context); a job that outlives its file and keeps its object until it ends; and the ABI program, whose
//! output must be CachyOS's (host/tests/dr/fixtures/abi-drm-cs.txt). Every test checks that no frame, object or
//! dma-buf is left.

use crate::fake::FakeGpu;
use crate::fence;
use crate::file::MinorKind;
use crate::ktests::{with_dev, Dev, KMem, BASE, KERNEL};
use crate::ktests_dr6::{create, put, take, Balance};
use crate::node::{open_device, OpenDrm};
use crate::uapi::*;
use crate::wire::Wire;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use amdgpu_fmt::pm4;
use ktest::{kassert, ktest, TestResult};

pub(crate) const V: u64 = 0x1_0000_0000;
pub(crate) const OUT: u64 = V + 0x800;
pub(crate) const RWX: u32 = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE | AMDGPU_VM_PAGE_EXECUTABLE;

pub(crate) fn bytes<T: Wire>(v: &T) -> Vec<u8> {
    let mut b = vec![0u8; T::SIZE];
    let _ = v.encode(&mut b);
    b
}

pub(crate) fn io(f: &OpenDrm, m: &KMem, cmd: u32, what: &str) -> Result<(), String> {
    f.ioctl_as(cmd, BASE, m, KERNEL).map(|_| ()).map_err(|e| format!("{what}: errno {e}"))
}

pub(crate) fn ctx_alloc(f: &OpenDrm, m: &KMem) -> Result<u32, String> {
    let mut a = drm_amdgpu_ctx::ZERO;
    a.set_in(&drm_amdgpu_ctx_in { op: AMDGPU_CTX_OP_ALLOC_CTX, flags: 0, ctx_id: 0, priority: 0 });
    put(m, 0, &a);
    io(f, m, DRM_IOCTL_AMDGPU_CTX, "CTX ALLOC")?;
    Ok(m.u64(0) as u32)
}

pub(crate) fn syncobj(f: &OpenDrm, m: &KMem) -> Result<u32, String> {
    put(m, 0, &drm_syncobj_create { handle: 0, flags: 0 });
    io(f, m, DRM_IOCTL_SYNCOBJ_CREATE, "SYNCOBJ_CREATE")?;
    take::<drm_syncobj_create>(m, 0).map(|s| s.handle).ok_or_else(|| "decode".into())
}

/// An object of two pages at V with `ib` at its start.
pub(crate) fn setup(f: &OpenDrm, m: &KMem, ib: &[u32]) -> Result<u32, String> {
    let h = create(f, m, 8192, AMDGPU_GEM_DOMAIN_GTT, 0)?;
    put(m, 0, &drm_amdgpu_gem_va { handle: h, operation: AMDGPU_VA_OP_MAP, flags: RWX, va_address: V, offset_in_bo: 0, map_size: 8192, ..drm_amdgpu_gem_va::ZERO });
    io(f, m, DRM_IOCTL_AMDGPU_GEM_VA, "GEM_VA")?;
    let b: Vec<u8> = ib.iter().flat_map(|d| d.to_le_bytes()).collect();
    kassert!(f.drm_file().bo(h).ok_or("no object")?.write(0, &b), "IB write");
    Ok(h)
}

pub(crate) fn ib_chunk(dw: usize) -> (u32, Vec<u8>) {
    (AMDGPU_CHUNK_ID_IB, bytes(&drm_amdgpu_cs_chunk_ib { _pad: 0, flags: 0, va_start: V, ib_bytes: dw as u32 * 4, ip_type: AMDGPU_HW_IP_GFX, ip_instance: 0, ring: 0 }))
}

fn cs(f: &OpenDrm, m: &KMem, ctx_id: u32, chunks: &[(u32, Vec<u8>)]) -> Result<u64, vfs::Errno> {
    cs_with(f, m, ctx_id, 0, chunks)
}

/// A CS naming BO list `list` (0: none).
pub(crate) fn cs_with(f: &OpenDrm, m: &KMem, ctx_id: u32, list: u32, chunks: &[(u32, Vec<u8>)]) -> Result<u64, vfs::Errno> {
    let n = chunks.len();
    let (ptrs, mut hdr, mut data) = (64usize, 64 + 8 * n, 64 + 24 * n);
    for (i, (id, d)) in chunks.iter().enumerate() {
        m.set(ptrs + 8 * i, &(BASE + hdr as u64).to_le_bytes());
        m.set(hdr, &bytes(&drm_amdgpu_cs_chunk { chunk_id: *id, length_dw: d.len() as u32 / 4, chunk_data: BASE + data as u64 }));
        m.set(data, d);
        hdr += 16;
        data += d.len().div_ceil(8) * 8;
    }
    let mut a = drm_amdgpu_cs::ZERO;
    a.set_in(&drm_amdgpu_cs_in { ctx_id, bo_list_handle: list, num_chunks: n as u32, flags: 0, chunks: BASE + ptrs as u64 });
    put(m, 0, &a);
    f.ioctl_as(DRM_IOCTL_AMDGPU_CS, BASE, m, KERNEL)?;
    Ok(m.u64(0))
}

pub(crate) fn wait_cs(f: &OpenDrm, m: &KMem, ctx_id: u32, seq: u64) -> Result<u64, vfs::Errno> {
    let mut a = drm_amdgpu_wait_cs::ZERO;
    a.set_in(&drm_amdgpu_wait_cs_in { handle: seq, timeout: 0, ip_type: 0, ip_instance: 0, ring: 0, ctx_id });
    put(m, 0, &a);
    f.ioctl_as(DRM_IOCTL_AMDGPU_WAIT_CS, BASE, m, KERNEL)?;
    Ok(m.u64(0))
}

pub(crate) fn program(val: u32, seq: u64) -> Vec<u32> {
    let mut p = Vec::new();
    let _ = pm4::write_data(&mut p, pm4::dst_sel::MEM, OUT, 0, &[val, !val]);
    let _ = pm4::fence(&mut p, OUT + 0x40, seq, true, false);
    p
}

/// Runs `t` on a fake device of its own when the test device is the fake (registered and removed around it).
pub(crate) fn own_device(t: impl FnOnce(&Arc<crate::device::Device>, &FakeGpu) -> TestResult) -> TestResult {
    own_device_with(FakeGpu::radeon_780m(), t)
}

/// `own_device` with a fake of the caller's (DR-9's fuzz: small memory pools).
pub(crate) fn own_device_with(g: FakeGpu, t: impl FnOnce(&Arc<crate::device::Device>, &FakeGpu) -> TestResult) -> TestResult {
    with_dev!(d, {
        if d.dev.driver().as_fake().is_none() {
            return ktest::skip("not the fake GPU");
        }
        let before = Balance::now();
        let gpu = Arc::new(g);
        let dev = crate::device::register(gpu.clone()).map_err(|e| format!("register: errno {e}"))?;
        let r = t(&dev, &gpu);
        crate::device::unregister(&dev);
        drop(dev);
        drop(gpu);
        r?;
        before.check()
    })
}

fn run() -> TestResult {
    own_device(|dev, gpu| {
        let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        let p = program(0x1234_5678, 0xfeed_0000_0000_0042);
        let h = setup(&f, &m, &p)?;
        let fbo = create(&f, &m, 4096, AMDGPU_GEM_DOMAIN_GTT, 0)?;
        let ufc = (AMDGPU_CHUNK_ID_FENCE, bytes(&drm_amdgpu_cs_chunk_fence { handle: fbo, offset: 8 }));
        for want in 1..=3u64 {
            let seq = cs(&f, &m, c, &[ib_chunk(p.len()), ufc.clone()]).map_err(|e| format!("CS: errno {e}"))?;
            kassert!(seq == want, "sequence {seq}, want {want}");
        }
        let bo = f.drm_file().bo(h).ok_or("no object")?;
        let mut b = [0u8; 8];
        kassert!(bo.read(0x800, &mut b) && b == [0x78, 0x56, 0x34, 0x12, 0x87, 0xa9, 0xcb, 0xed], "WRITE_DATA bytes {b:x?}");
        kassert!(bo.read(0x840, &mut b) && u64::from_le_bytes(b) == 0xfeed_0000_0000_0042, "RELEASE_MEM bytes {b:x?}");
        kassert!(f.drm_file().bo(fbo).ok_or("no fence object")?.read(8, &mut b) && u64::from_le_bytes(b) == 3, "user fence {b:x?}");
        kassert!(wait_cs(&f, &m, c, 3) == Ok(0), "WAIT_CS on a finished job");
        kassert!(wait_cs(&f, &m, c, 4) == Err(linux::errno::EINVAL), "WAIT_CS beyond the last number");
        let list: Vec<u8> = (1..=3u64).flat_map(|s| bytes(&drm_amdgpu_fence { ctx_id: c, ip_type: 0, ip_instance: 0, ring: 0, seq_no: s })).collect();
        m.set(4096, &list);
        let mut a = drm_amdgpu_wait_fences::ZERO;
        a.set_in(&drm_amdgpu_wait_fences_in { fences: BASE + 4096, fence_count: 3, wait_all: 1, timeout_ns: 0 });
        put(&m, 0, &a);
        io(&f, &m, DRM_IOCTL_AMDGPU_WAIT_FENCES, "WAIT_FENCES")?;
        kassert!(m.u64(0) & 0xffff_ffff == 1, "WAIT_FENCES status");
        kassert!(gpu.cs_stats() == (3, 0, 0), "stats {:?}", gpu.cs_stats());
        Ok(())
    })
}
ktest!("dr.cs.run", Safe, run);

fn hang() -> TestResult {
    own_device(|dev, gpu| {
        let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        setup(&f, &m, &[0xc000_ff00, 0])?;
        let seq = cs(&f, &m, c, &[ib_chunk(2)]).map_err(|e| format!("CS: errno {e}"))?;
        kassert!(wait_cs(&f, &m, c, seq) == Err(linux::errno::ETIME), "the hung job's error");
        kassert!(cs(&f, &m, c, &[ib_chunk(2)]) == Err(linux::ab::errno_more::ECANCELED), "a guilty context");
        kassert!(gpu.cs_stats().1 == 1, "one hang");
        Ok(())
    })
}
ktest!("dr.cs.hang", Safe, hang);

/// A job waiting for a fence when its file closes: the close waits its second, the job ends with ESRCH when
/// the fence comes, and its object stays until then.
fn dying_fd() -> TestResult {
    own_device(|dev, gpu| {
        let f = open_device(dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
        let m = KMem::new(8192);
        let c = ctx_alloc(&f, &m)?;
        let p = program(7, 7);
        let h = setup(&f, &m, &p)?;
        let (sin, sout) = (syncobj(&f, &m)?, syncobj(&f, &m)?);
        let gate = gpu.ring().emit();
        f.drm_file().syncobj(sin).ok_or("no syncobj")?.replace_fence(Some(gate));
        let sem = |id: u32, h: u32| (id, h.to_le_bytes().to_vec());
        m.set(6144, &bytes(&drm_amdgpu_bo_list_entry { bo_handle: h, bo_priority: 0 }));
        let bl = (AMDGPU_CHUNK_ID_BO_HANDLES, bytes(&drm_amdgpu_bo_list_in { operation: 0, list_handle: 0, bo_number: 1, bo_info_size: 8, bo_info_ptr: BASE + 6144 }));
        cs(&f, &m, c, &[ib_chunk(p.len()), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, sin), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, sout), bl])
            .map_err(|e| format!("CS: errno {e}"))?;
        let out = f.drm_file().syncobj(sout).and_then(|o| o.fence()).ok_or("no out fence")?;
        let weak = Arc::downgrade(&f.drm_file().bo(h).ok_or("no object")?);
        drop(f);
        kassert!(weak.upgrade().is_some() && !fence::is_signaled(&out), "the job waits and holds its object");
        gpu.ring().signal_all();
        kassert!(fence::status(&out) == -(linux::errno::ESRCH as i32), "status {}", fence::status(&out));
        kassert!(weak.upgrade().is_none(), "the object went with the job");
        kassert!(gpu.cs_stats().2 == 0, "nothing queued");
        Ok(())
    })
}
ktest!("dr.cs.dying_fd", Safe, dying_fd);

const PROG: &str = "/bin/abi-drm-cs";
const EXPECTED: &str = include_str!("../../../host/tests/dr/fixtures/abi-drm-cs.txt");

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
        let argv = ["abi-drm-cs"];
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
ktest!("dr.cs.program", Safe, abi_program);
