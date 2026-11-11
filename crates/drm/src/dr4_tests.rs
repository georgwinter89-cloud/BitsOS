//! DR-4 host tests: AMDGPU_INFO against the fake gives byte for byte what Linux gave Mesa on the 780M (the
//! captured answers in host/tests/dr/fixtures/780m-info.txt), Linux 6.6's errors for the rest, memory numbers for
//! any carve-out size, and AMDGPU_CTX (ids, priorities, reset queries, stable pstate, end at close).

use crate::device;
use crate::dispatch::Caller;
use crate::dr2_tests::MINORS;
use crate::fake::{FakeGpu, FakeMemory};
use crate::file::MinorKind;
use crate::gpu::GpuDevice;
use crate::node::{open_device, OpenDrm};
use crate::uapi::*;
use crate::wire::Wire;
use linux::errno::*;
use std::sync::{Arc, Mutex};
use std::vec;
use std::vec::Vec;

const BASE: u64 = 0x10000;
const OUT: usize = 1024;
const ME: Caller = Caller { pid: 42, sys_admin: false, sys_nice: false };
const NICE: Caller = Caller { pid: 43, sys_admin: false, sys_nice: true };
const FIXTURE: &str = include_str!("../../../host/tests/dr/fixtures/780m-info.txt");

struct Mem(Mutex<Vec<u8>>);

impl Mem {
    fn new() -> Mem {
        Mem(Mutex::new(vec![0xa5; 8192]))
    }
    fn set(&self, off: usize, d: &[u8]) {
        self.0.lock().unwrap()[off..off + d.len()].copy_from_slice(d);
    }
    fn get(&self, off: usize, n: usize) -> Vec<u8> {
        self.0.lock().unwrap()[off..off + n].to_vec()
    }
    fn u32(&self, off: usize) -> u32 {
        u32::from_le_bytes(self.get(off, 4).try_into().unwrap())
    }
    fn u64(&self, off: usize) -> u64 {
        u64::from_le_bytes(self.get(off, 8).try_into().unwrap())
    }
    fn range(&self, addr: u64, n: usize) -> Option<core::ops::Range<usize>> {
        let s = usize::try_from(addr.checked_sub(BASE)?).ok()?;
        (s.checked_add(n)? <= self.0.lock().unwrap().len()).then_some(s..s + n)
    }
}

impl vfs::sys::UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let Some(r) = self.range(addr, buf.len()) else { return false };
        buf.copy_from_slice(&self.0.lock().unwrap()[r]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let Some(r) = self.range(addr, data.len()) else { return false };
        self.0.lock().unwrap()[r].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

struct Fx {
    gpu: Arc<FakeGpu>,
    f: Option<Arc<OpenDrm>>,
    dev: Arc<device::Device>,
}

impl Fx {
    fn new() -> Fx {
        Fx::with(FakeGpu::radeon_780m())
    }
    fn with(g: FakeGpu) -> Fx {
        let gpu = Arc::new(g);
        let dev = {
            let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
            device::register(gpu.clone()).unwrap()
        };
        let f = open_device(dev.clone(), MinorKind::Render, false).unwrap();
        Fx { gpu, f: Some(f), dev }
    }
    fn file(&self) -> &OpenDrm {
        self.f.as_ref().unwrap()
    }
    /// INFO with the answer at BASE + OUT.
    fn info(&self, m: &Mem, query: u32, a: [u32; 4], size: u32) -> Result<i64, vfs::Errno> {
        self.info_at(m, query, a, size, BASE + OUT as u64)
    }
    fn info_at(&self, m: &Mem, query: u32, a: [u32; 4], size: u32, ptr: u64) -> Result<i64, vfs::Errno> {
        let mut req = drm_amdgpu_info::ZERO;
        req.return_pointer = ptr;
        req.return_size = size;
        req.query = query;
        a.encode(&mut req.anon0.bytes).unwrap();
        let mut b = [0u8; 32];
        req.encode(&mut b).unwrap();
        m.set(0, &b);
        self.file().ioctl_as(DRM_IOCTL_AMDGPU_INFO, BASE, m, ME)
    }
    fn ctx(&self, m: &Mem, op: u32, flags: u32, id: u32, prio: i32, who: Caller) -> Result<i64, vfs::Errno> {
        let a = drm_amdgpu_ctx_in { op, flags, ctx_id: id, priority: prio };
        let mut b = [0u8; 16];
        a.encode(&mut b).unwrap();
        m.set(256, &b);
        self.file().ioctl_as(DRM_IOCTL_AMDGPU_CTX, BASE + 256, m, who)
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.f = None;
        let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
        device::unregister(&self.dev);
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// The fixture's lines: (query, args, size, answer).
fn captured() -> Vec<(u32, [u32; 4], u32, Vec<u8>)> {
    FIXTURE
        .lines()
        .filter(|l| l.starts_with("Q "))
        .map(|l| {
            let w: Vec<&str> = l.split_whitespace().collect();
            let q = u32::from_str_radix(w[1].trim_start_matches("0x"), 16).unwrap();
            let a: Vec<u32> = w[2].trim_start_matches("a=").split(',').map(|x| x.parse().unwrap()).collect();
            let (sz, data) = w[3].trim_start_matches("sz=").split_once(':').unwrap();
            (q, [a[0], a[1], a[2], a[3]], sz.parse().unwrap(), hex(data))
        })
        .collect()
}

#[test]
fn every_captured_answer_is_linuxs() {
    let fx = Fx::new();
    let all = captured();
    assert_eq!(all.len(), 20);
    for (q, a, sz, want) in all {
        let m = Mem::new();
        assert_eq!(fx.info(&m, q, a, sz), Ok(0), "query {q:#x} {a:?}");
        assert_eq!(m.get(OUT, sz as usize), want, "query {q:#x} {a:?}");
        // Nothing past return_size, and the request is not written back.
        assert_eq!(m.get(OUT + sz as usize, 8), vec![0xa5; 8], "query {q:#x} wrote past its size");
        assert_eq!(m.u32(12), q);
    }
}

#[test]
fn scalars_mesa_reads() {
    let fx = Fx::new();
    let m = Mem::new();
    assert_eq!(fx.info(&m, AMDGPU_INFO_ACCEL_WORKING, [0; 4], 4), Ok(0));
    assert_eq!(m.u32(OUT), 1);
    // GB_ADDR_CONFIG as libdrm reads it (one dword, all SEs and SHs).
    assert_eq!(fx.info(&m, AMDGPU_INFO_READ_MMR_REG, [0x263e, 1, 0xffff_ffff, 0], 4), Ok(0));
    assert_eq!(m.u32(OUT), 0x242);
    let count: Vec<u32> = (0..10)
        .map(|t| {
            assert_eq!(fx.info(&m, AMDGPU_INFO_HW_IP_COUNT, [t, 0, 0, 0], 4), Ok(0));
            m.u32(OUT)
        })
        .collect();
    assert_eq!(count, [1, 1, 1, 0, 0, 0, 1, 1, 1, 0]);
    assert_eq!(fx.info(&m, AMDGPU_INFO_HW_IP_COUNT, [10, 0, 0, 0], 4), Err(EINVAL));
    // Usage after device creation: the kernel driver's own buffers.
    for (q, v) in [(AMDGPU_INFO_VRAM_USAGE, 0x530_6000u64), (AMDGPU_INFO_VIS_VRAM_USAGE, 0x530_6000), (AMDGPU_INFO_GTT_USAGE, 0xd8_8000)] {
        assert_eq!(fx.info(&m, q, [0; 4], 8), Ok(0));
        assert_eq!(m.u64(OUT), v, "query {q:#x}");
    }
    // DR-6: a 1 MiB VRAM object counts in, its close takes it out again.
    let mut c = drm_amdgpu_gem_create::ZERO;
    c.set_in(&drm_amdgpu_gem_create_in { bo_size: 1 << 20, alignment: 4096, domains: AMDGPU_GEM_DOMAIN_VRAM as u64, domain_flags: 0 });
    let mut b = [0u8; 32];
    c.encode(&mut b).unwrap();
    m.set(512, &b);
    assert_eq!(fx.file().ioctl_as(DRM_IOCTL_AMDGPU_GEM_CREATE, BASE + 512, &m, ME), Ok(0));
    let h = m.u32(512);
    assert_eq!(fx.info(&m, AMDGPU_INFO_VRAM_USAGE, [0; 4], 8), Ok(0));
    assert_eq!(m.u64(OUT), 0x530_6000 + (1 << 20));
    assert_eq!(fx.file().drm_file().handle_delete(h), Ok(()));
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
    // A 4-byte buffer gets the low half of a u64 answer.
    m.set(OUT, &[0xa5; 8]);
    assert_eq!(fx.info(&m, AMDGPU_INFO_GTT_USAGE, [0; 4], 4), Ok(0));
    assert_eq!(m.get(OUT, 8), [0, 0x80, 0xd8, 0, 0xa5, 0xa5, 0xa5, 0xa5]);
    assert_eq!(fx.info(&m, AMDGPU_INFO_VRAM_LOST_COUNTER, [0; 4], 4), Ok(0));
    assert_eq!(m.u32(OUT), 0);
    assert_eq!(fx.info(&m, AMDGPU_INFO_GDS_CONFIG, [0; 4], 32), Ok(0));
    assert_eq!((m.u32(OUT + 4), m.u32(OUT + 8), m.u32(OUT + 16), m.u32(OUT + 24)), (0x1000, 0x1000, 64, 16));
    assert_eq!(fx.info(&m, AMDGPU_INFO_SENSOR, [AMDGPU_INFO_SENSOR_GFX_SCLK, 0, 0, 0], 4), Ok(0));
    assert_eq!(m.u32(OUT), 800);
    assert_eq!(fx.info(&m, AMDGPU_INFO_SENSOR, [AMDGPU_INFO_SENSOR_GPU_TEMP, 0, 0, 0], 4), Ok(0));
    assert_eq!(m.u32(OUT), 39_000);
    assert_eq!(fx.info(&m, AMDGPU_INFO_SENSOR, [AMDGPU_INFO_SENSOR_PEAK_PSTATE_GFX_SCLK, 0, 0, 0], 4), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_SENSOR, [0xc, 0, 0, 0], 4), Err(EINVAL));
    // TIMESTAMP counts up at about 100 MHz.
    assert_eq!(fx.info(&m, AMDGPU_INFO_TIMESTAMP, [0; 4], 8), Ok(0));
    let t0 = m.u64(OUT);
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(fx.info(&m, AMDGPU_INFO_TIMESTAMP, [0; 4], 8), Ok(0));
    assert!(m.u64(OUT) > t0 + 100_000, "{} -> {}", t0, m.u64(OUT));
}

#[test]
fn linux_errors() {
    let fx = Fx::new();
    let m = Mem::new();
    let at = |ptr: u64, size: u32, q: u32| fx.info_at(&m, q, [0; 4], size, ptr);
    assert_eq!(at(BASE + 1024, 0, AMDGPU_INFO_DEV_INFO), Err(EINVAL));
    assert_eq!(at(0, 448, AMDGPU_INFO_DEV_INFO), Err(EINVAL));
    assert_eq!(at(0x7000_0000, 448, AMDGPU_INFO_DEV_INFO), Err(EFAULT));
    assert_eq!(at(BASE + 1024, 4, 0x7f), Err(EINVAL));
    assert_eq!(at(BASE + 1024, 4, AMDGPU_INFO_CRTC_FROM_ID), Err(EINVAL));
    // A short buffer gets the start of the answer.
    assert_eq!(fx.info(&m, AMDGPU_INFO_DEV_INFO, [0; 4], 6), Ok(0));
    assert_eq!(m.get(OUT, 8), [0x00, 0x19, 0, 0, 0x0c, 0, 0xa5, 0xa5]);
    // HW_IP_INFO: instance 1 and unknown types EINVAL (absent types give zeros, see the fixture).
    assert_eq!(fx.info(&m, AMDGPU_INFO_HW_IP_INFO, [0, 1, 0, 0], 32), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_HW_IP_INFO, [10, 0, 0, 0], 32), Err(EINVAL));
    // FW_VERSION: instance, unknown type (VPE is newer than 6.6), MEC 2, SDMA 1, TA index 1 (ASD).
    for a in [
        [AMDGPU_INFO_FW_GFX_ME, 1, 0, 0],
        [AMDGPU_INFO_FW_VPE, 0, 0, 0],
        [AMDGPU_INFO_FW_GFX_MEC, 0, 2, 0],
        [AMDGPU_INFO_FW_SDMA, 0, 1, 0],
        [AMDGPU_INFO_FW_TA, 0, 1, 0],
    ] {
        assert_eq!(fx.info(&m, AMDGPU_INFO_FW_VERSION, a, 8), Err(EINVAL), "{a:?}");
    }
    assert_eq!(fx.info(&m, AMDGPU_INFO_FW_VERSION, [AMDGPU_INFO_FW_SDMA, 0, 0, 0], 8), Ok(0));
    assert_eq!((m.u32(OUT), m.u32(OUT + 4)), (0x18, 60));
    // READ_MMR_REG: SE/SH out of range, too many, a register outside the list (EFAULT, as Linux).
    assert_eq!(fx.info(&m, AMDGPU_INFO_READ_MMR_REG, [0x263e, 1, 4, 0], 4), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_READ_MMR_REG, [0x263e, 1, 0x0200, 0], 4), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_READ_MMR_REG, [0x263e, 129, 0xffff_ffff, 0], 4), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_READ_MMR_REG, [0x263d, 2, 0xffff_ffff, 0], 8), Err(EFAULT));
    assert_eq!(fx.info(&m, AMDGPU_INFO_READ_MMR_REG, [0x263e, 1, 0x0103, 0], 4), Ok(0));
    assert_eq!(fx.info(&m, AMDGPU_INFO_READ_MMR_REG, [0x263e, 0, 0xffff_ffff, 0], 4), Ok(0));
    // VBIOS: no image, the part number.
    assert_eq!(fx.info(&m, AMDGPU_INFO_VBIOS, [AMDGPU_INFO_VBIOS_SIZE, 0, 0, 0], 4), Ok(0));
    assert_eq!(m.u32(OUT), 0);
    assert_eq!(fx.info(&m, AMDGPU_INFO_VBIOS, [AMDGPU_INFO_VBIOS_IMAGE, 0, 0, 0], 4), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_VBIOS, [AMDGPU_INFO_VBIOS_INFO, 0, 0, 0], 200), Ok(0));
    assert_eq!(&m.get(OUT + 64, 19), b"113-PHXGENERIC-001\0");
    assert_eq!(fx.info(&m, AMDGPU_INFO_VBIOS, [9, 0, 0, 0], 4), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_NUM_HANDLES, [AMDGPU_HW_IP_UVD, 0, 0, 0], 8), Err(ENODATA));
    assert_eq!(fx.info(&m, AMDGPU_INFO_NUM_HANDLES, [AMDGPU_HW_IP_GFX, 0, 0, 0], 8), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_VIDEO_CAPS, [2, 0, 0, 0], 192), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_RAS_ENABLED_FEATURES, [0; 4], 8), Err(EINVAL));
    assert_eq!(fx.info(&m, AMDGPU_INFO_VCE_CLOCK_TABLE, [0; 4], 8), Ok(0));
    assert_eq!(m.u64(OUT), 0);
}

#[test]
fn any_carveout_size() {
    const MIB: u64 = 1 << 20;
    for carve in [4 * MIB, 512 * MIB, 2048 * MIB, 8192 * MIB, 16384 * MIB, 6 * 1024 * MIB + 4096] {
        let fx = Fx::with(FakeGpu::radeon_780m_with(FakeMemory::with_carveout(carve)));
        let m = Mem::new();
        assert_eq!(fx.info(&m, AMDGPU_INFO_MEMORY, [0; 4], 96), Ok(0));
        let mi = drm_amdgpu_memory_info::decode(&m.get(OUT, 96)).unwrap();
        let usable = carve.saturating_sub(0x52f_d000 + 8 * MIB);
        assert_eq!(mi.vram.total_heap_size, carve);
        assert_eq!(mi.vram.usable_heap_size, usable, "carve-out {carve:#x}");
        assert_eq!(mi.vram.max_allocation, (usable as u128 * 3 / 4) as u64);
        assert_eq!(mi.cpu_accessible_vram.total_heap_size, carve);
        assert!(mi.cpu_accessible_vram.usable_heap_size <= usable);
        assert_eq!(mi.gtt.usable_heap_size, 0x3_d035_3000 - 0xd8_8000);
        assert_eq!(fx.info(&m, AMDGPU_INFO_VRAM_GTT, [0; 4], 24), Ok(0));
        assert_eq!((m.u64(OUT), m.u64(OUT + 8)), (usable, mi.cpu_accessible_vram.usable_heap_size));
    }
}

#[test]
fn ctx_ids_and_priorities() {
    let fx = Fx::new();
    let m = Mem::new();
    let alloc = |prio: i32, who: Caller| fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, prio, who).map(|_| m.u32(256));
    assert_eq!(alloc(0, ME), Ok(1));
    assert_eq!(alloc(0, ME), Ok(2));
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_FREE_CTX, 0, 1, 0, ME), Ok(0));
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_FREE_CTX, 0, 1, 0, ME), Err(EINVAL));
    assert_eq!(alloc(0, ME), Ok(1), "lowest free id again");
    // The flags word (bytes 4..8) stays as it came: Linux writes only ctx_id.
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 7, 0, 0, ME), Ok(0));
    assert_eq!((m.u32(256), m.u32(260)), (3, 7));
    // Priorities: below and at NORMAL for everyone, above only with CAP_SYS_NICE; garbage counts as NORMAL.
    assert_eq!(alloc(AMDGPU_CTX_PRIORITY_LOW, ME), Ok(4));
    assert_eq!(alloc(AMDGPU_CTX_PRIORITY_VERY_LOW, ME), Ok(5));
    assert_eq!(alloc(AMDGPU_CTX_PRIORITY_HIGH as i32, ME), Err(EACCES));
    assert_eq!(m.u32(256), 0, "a refused ALLOC answers ctx_id 0");
    assert_eq!(alloc(AMDGPU_CTX_PRIORITY_VERY_HIGH as i32, ME), Err(EACCES));
    assert_eq!(alloc(77, ME), Ok(6));
    assert_eq!(alloc(AMDGPU_CTX_PRIORITY_UNSET, ME), Ok(7));
    assert_eq!(alloc(AMDGPU_CTX_PRIORITY_VERY_HIGH as i32, NICE), Ok(8));
    assert_eq!(fx.ctx(&m, 9, 0, 1, 0, ME), Err(EINVAL));
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_QUERY_STATE2, 0, 99, 0, ME), Err(EINVAL));
    assert_eq!(fx.file().drm_file().gpu_ctxs.lock().len(), 8);
}

#[test]
fn ctx_reset_queries() {
    let fx = Fx::new();
    let m = Mem::new();
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    let state = |op: u32| {
        fx.ctx(&m, op, 0, 1, 0, ME).unwrap();
        (m.u64(256), m.u32(264), m.u32(268))
    };
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2), (0, 0, 0));
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE), (0, 0, AMDGPU_CTX_NO_RESET));
    fx.gpu.reset(true);
    let both = (AMDGPU_CTX_QUERY2_FLAGS_RESET | AMDGPU_CTX_QUERY2_FLAGS_VRAMLOST) as u64;
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2), (both, 0, 0));
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE), (0, 0, AMDGPU_CTX_UNKNOWN_RESET));
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE), (0, 0, AMDGPU_CTX_NO_RESET));
    // QUERY_STATE2 keeps reporting: it compares with the counters at creation.
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2), (both, 0, 0));
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    fx.ctx(&m, AMDGPU_CTX_OP_QUERY_STATE2, 0, 2, 0, ME).unwrap();
    assert_eq!(m.u64(256), 0, "a context made after the reset is healthy");
}

/// GR-3 CR 1: a queue-scope blame moves no device counter, the blamed context still reads RESET | GUILTY, the old
/// query says UNKNOWN_RESET once, another context reads nothing.
#[test]
fn ctx_blame_without_a_counter_move() {
    let fx = Fx::new();
    let m = Mem::new();
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    let state = |op: u32, id: u32| {
        fx.ctx(&m, op, 0, id, 0, ME).unwrap();
        (m.u64(256), m.u32(268))
    };
    let before = fx.gpu.reset_counter();
    let c = crate::ctx::lookup(fx.file().drm_file(), 1).unwrap();
    let e = c.entity(&*fx.gpu, AMDGPU_HW_IP_GFX, 0, 0).unwrap();
    e.job.blame(fx.gpu.reset_counter());
    assert_eq!(fx.gpu.reset_counter(), before);
    let told = (AMDGPU_CTX_QUERY2_FLAGS_RESET | AMDGPU_CTX_QUERY2_FLAGS_GUILTY) as u64;
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2, 1).0, told);
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE, 1).1, AMDGPU_CTX_UNKNOWN_RESET);
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE, 1).1, AMDGPU_CTX_NO_RESET);
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2, 1).0, told, "the flags stay, as after a real move");
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2, 2).0, 0, "the other context was not touched");
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE, 2).1, AMDGPU_CTX_NO_RESET);
    // A real reset after it: the snapshot was off already, the rule holds.
    e.job.blame(fx.gpu.reset_counter());
    fx.gpu.reset(false);
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2, 2).0, AMDGPU_CTX_QUERY2_FLAGS_RESET as u64);
    assert_eq!(state(AMDGPU_CTX_OP_QUERY_STATE2, 1).0, told);
}

#[test]
fn ctx_stable_pstate() {
    let fx = Fx::new();
    let m = Mem::new();
    for _ in 0..2 {
        assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    }
    let get = |id: u32| fx.ctx(&m, AMDGPU_CTX_OP_GET_STABLE_PSTATE, 0, id, 0, ME).map(|_| m.u32(256));
    let set = |id: u32, p: u32| fx.ctx(&m, AMDGPU_CTX_OP_SET_STABLE_PSTATE, p, id, 0, ME);
    assert_eq!(get(1), Ok(AMDGPU_CTX_STABLE_PSTATE_NONE));
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_GET_STABLE_PSTATE, 1, 1, 0, ME), Err(EINVAL));
    assert_eq!(get(9), Err(EINVAL));
    assert_eq!(set(1, 0x10), Err(EINVAL));
    assert_eq!(set(1, 5), Err(EINVAL));
    assert_eq!(set(1, AMDGPU_CTX_STABLE_PSTATE_PEAK), Ok(0));
    assert_eq!(get(2), Ok(AMDGPU_CTX_STABLE_PSTATE_PEAK));
    assert_eq!(set(2, AMDGPU_CTX_STABLE_PSTATE_STANDARD), Err(EBUSY));
    assert_eq!(set(1, AMDGPU_CTX_STABLE_PSTATE_MIN_SCLK), Ok(0));
    assert_eq!(fx.gpu.stable_pstate(), AMDGPU_CTX_STABLE_PSTATE_MIN_SCLK);
    // The owner ends: the pstate it found (NONE) comes back and the other context may set one.
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_FREE_CTX, 0, 1, 0, ME), Ok(0));
    assert_eq!(fx.gpu.stable_pstate(), AMDGPU_CTX_STABLE_PSTATE_NONE);
    assert_eq!(set(2, AMDGPU_CTX_STABLE_PSTATE_STANDARD), Ok(0));
    assert_eq!(fx.gpu.stable_pstate(), AMDGPU_CTX_STABLE_PSTATE_STANDARD);
}

#[test]
fn close_ends_contexts_and_unplug_refuses() {
    let mut fx = Fx::new();
    let m = Mem::new();
    for _ in 0..3 {
        assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    }
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_SET_STABLE_PSTATE, AMDGPU_CTX_STABLE_PSTATE_PEAK, 3, 0, ME), Ok(0));
    fx.f = None;
    assert_eq!(fx.gpu.stable_pstate(), AMDGPU_CTX_STABLE_PSTATE_NONE, "the last close gives the pstate back");
    // A second file starts with id 1; after unplug INFO and CTX answer ENODEV.
    fx.f = Some(open_device(fx.dev.clone(), MinorKind::Render, false).unwrap());
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    assert_eq!(m.u32(256), 1);
    device::unplug(&fx.dev);
    assert_eq!(fx.info(&m, AMDGPU_INFO_ACCEL_WORKING, [0; 4], 4), Err(ENODEV));
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_FREE_CTX, 0, 1, 0, ME), Err(ENODEV));
}

/// Checker DR-4: the SMU call runs outside the pstate lock; a failed change leaves the owner as it was; a failed
/// release at the end still lets go (a gone context must not block the others for good).
#[test]
fn ctx_stable_pstate_failures() {
    let fx = Fx::new();
    let m = Mem::new();
    for _ in 0..2 {
        assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_ALLOC_CTX, 0, 0, 0, ME), Ok(0));
    }
    let set = |id: u32, p: u32| fx.ctx(&m, AMDGPU_CTX_OP_SET_STABLE_PSTATE, p, id, 0, ME);
    fx.gpu.fail_next_pstate(ETIME);
    assert_eq!(set(1, AMDGPU_CTX_STABLE_PSTATE_PEAK), Err(ETIME));
    assert_eq!(fx.gpu.stable_pstate(), AMDGPU_CTX_STABLE_PSTATE_NONE);
    assert_eq!(set(2, AMDGPU_CTX_STABLE_PSTATE_STANDARD), Ok(0), "a failed force makes no owner");
    fx.gpu.fail_next_pstate(EIO);
    assert_eq!(set(2, AMDGPU_CTX_STABLE_PSTATE_NONE), Err(EIO));
    assert_eq!(set(1, AMDGPU_CTX_STABLE_PSTATE_PEAK), Err(EBUSY), "a failed release keeps the owner");
    fx.gpu.fail_next_pstate(EIO);
    assert_eq!(fx.ctx(&m, AMDGPU_CTX_OP_FREE_CTX, 0, 2, 0, ME), Ok(0));
    assert_eq!(set(1, AMDGPU_CTX_STABLE_PSTATE_PEAK), Ok(0), "the gone owner let go");
    assert!(!fx.gpu.pstate_changed_under_lock());
}

/// Checker DR-4: unknown INFO queries chosen by a program cannot grow the device's Warn-once set without end.
#[test]
fn unknown_queries_stay_bounded() {
    let fx = Fx::new();
    let m = Mem::new();
    for q in 0x1000..0x1000 + 4 * device::UNKNOWN_REMEMBERED as u32 {
        assert_eq!(fx.info(&m, q, [0; 4], 4), Err(EINVAL));
    }
    assert_eq!(fx.dev.unknown_remembered(), device::UNKNOWN_REMEMBERED);
    assert_eq!(fx.info(&m, AMDGPU_INFO_ACCEL_WORKING, [0; 4], 4), Ok(0), "known queries still answer");
}
