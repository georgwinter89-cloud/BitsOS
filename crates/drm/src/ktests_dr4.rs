//! DR-4 in-kernel tests (Safe, rerunnable): AMDGPU_INFO through an open file of the boot device answers what
//! Linux answered on the 780M (the captured fixture, when the device is the fake with its captured memory), and
//! AMDGPU_CTX gives ids, refuses nothing to the kernel, and ends its contexts at close. On hardware without a
//! fake they fail, as DR-2's do.

use crate::file::MinorKind;
use crate::gpu::GpuDevice;
use crate::ktests::{with_dev, Dev, KMem, BASE, KERNEL};
use crate::node::open_device;
use crate::uapi::*;
use crate::wire::Wire;
use alloc::format;
use alloc::vec::Vec;
use ktest::{kassert, ktest, TestResult};

const FIXTURE: &str = include_str!("../../../host/tests/dr/fixtures/780m-info.txt");
const OUT: usize = 1024;

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).filter_map(|i| u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()).collect()
}

fn u32_at(m: &KMem, off: usize) -> u32 {
    u32::from_le_bytes(m.at(off, 4).try_into().unwrap_or([0; 4]))
}

fn info(f: &crate::node::OpenDrm, m: &KMem, query: u32, a: [u32; 4], size: u32) -> Result<i64, vfs::Errno> {
    let mut req = drm_amdgpu_info::ZERO;
    req.return_pointer = BASE + OUT as u64;
    req.return_size = size;
    req.query = query;
    let _ = a.encode(&mut req.anon0.bytes);
    let mut b = [0u8; 32];
    let _ = req.encode(&mut b);
    m.set(0, &b);
    f.ioctl_as(DRM_IOCTL_AMDGPU_INFO, BASE, m, KERNEL)
}

fn captured_answers() -> TestResult {
    with_dev!(d, {
        let Some(fake) = d.dev.driver().as_fake() else { return ktest::fail("the device is no fake: no fixture to compare") };
        if fake.memory().vram_total != 0x2000_0000 {
            return ktest::fail("the fake has another carve-out than the capture");
        }
        let f = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
        let m = KMem::new(4096);
        let mut n = 0;
        for l in FIXTURE.lines().filter(|l| l.starts_with("Q ")) {
            let w: Vec<&str> = l.split_whitespace().collect();
            kassert!(w.len() == 4, "fixture line {l}");
            let q = u32::from_str_radix(w[1].trim_start_matches("0x"), 16).unwrap_or(u32::MAX);
            let a: Vec<u32> = w[2].trim_start_matches("a=").split(',').filter_map(|x| x.parse().ok()).collect();
            let (sz, data) = w[3].trim_start_matches("sz=").split_once(':').unwrap_or(("0", ""));
            let sz: u32 = sz.parse().unwrap_or(0);
            kassert!(a.len() == 4 && sz > 0, "fixture line {l}");
            m.set(OUT, &[0xa5; 512]);
            let r = info(&f, &m, q, [a[0], a[1], a[2], a[3]], sz);
            kassert!(r == Ok(0), "query {q:#x} {a:?}: {r:?}");
            kassert!(m.at(OUT, sz as usize) == hex(data), "query {q:#x} {a:?}: answer differs from Linux's");
            kassert!(m.at(OUT + sz as usize, 4) == [0xa5; 4], "query {q:#x} wrote past return_size");
            n += 1;
        }
        kassert!(n == 20, "{n} fixture lines");
        kassert!(info(&f, &m, AMDGPU_INFO_ACCEL_WORKING, [0; 4], 4) == Ok(0) && u32_at(&m, OUT) == 1);
        kassert!(info(&f, &m, AMDGPU_INFO_READ_MMR_REG, [0x263e, 1, u32::MAX, 0], 4) == Ok(0) && u32_at(&m, OUT) == 0x242);
        kassert!(info(&f, &m, 0x7f, [0; 4], 4) == Err(linux::errno::EINVAL));
        Ok(())
    })
}
ktest!("dr.info.captured", QemuAll, captured_answers);

fn ctx(f: &crate::node::OpenDrm, m: &KMem, op: u32, id: u32, prio: i32) -> Result<i64, vfs::Errno> {
    let mut b = [0u8; 16];
    let _ = drm_amdgpu_ctx_in { op, flags: 0, ctx_id: id, priority: prio }.encode(&mut b);
    m.set(256, &b);
    f.ioctl_as(DRM_IOCTL_AMDGPU_CTX, BASE + 256, m, KERNEL)
}

fn contexts() -> TestResult {
    with_dev!(d, {
        let Some(gpu) = d.dev.driver().gpu() else { return ktest::fail("the device has no amdgpu uAPI") };
        let f = open_device(d.dev.clone(), MinorKind::Render, false).map_err(|e| format!("open: errno {e}"))?;
        let m = KMem::new(4096);
        // The kernel may ask for any priority (CAP_SYS_NICE).
        for (want, prio) in [(1, 0), (2, AMDGPU_CTX_PRIORITY_LOW), (3, AMDGPU_CTX_PRIORITY_VERY_HIGH as i32)] {
            let r = ctx(&f, &m, AMDGPU_CTX_OP_ALLOC_CTX, 0, prio);
            kassert!(r == Ok(0) && u32_at(&m, 256) == want, "alloc prio {prio}: {r:?} id {}", u32_at(&m, 256));
        }
        kassert!(ctx(&f, &m, AMDGPU_CTX_OP_QUERY_STATE2, 2, 0) == Ok(0) && m.u64(256) == 0, "query2 flags {:#x}", m.u64(256));
        kassert!(ctx(&f, &m, AMDGPU_CTX_OP_FREE_CTX, 2, 0) == Ok(0));
        kassert!(ctx(&f, &m, AMDGPU_CTX_OP_FREE_CTX, 2, 0) == Err(linux::errno::EINVAL));
        kassert!(f.drm_file().gpu_ctxs.lock().len() == 2);
        drop(f);
        kassert!(gpu.stable_pstate() == AMDGPU_CTX_STABLE_PSTATE_NONE);
        Ok(())
    })
}
ktest!("dr.ctx.alloc_free", QemuAll, contexts);
