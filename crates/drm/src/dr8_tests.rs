//! DR-8 host tests: AMDGPU_CS's checks in Linux 6.6's order (amdgpu_cs.c), jobs that run on the fake's PM4
//! interpreter and write memory byte for byte, the user fence, sequence numbers per entity, dependencies by
//! sequence number, syncobjs (in, out, timeline wait and signal), implicit sync, priorities, gangs, hangs (ETIME,
//! guilty context, QUERY_STATE2), WAIT_CS and WAIT_FENCES with absolute deadlines, and jobs that outlive their
//! file: they finish (or end with ESRCH) and keep their memory until then.

use crate::dispatch::Caller;
use crate::dr6_tests::{Fx, BASE, ME};
use crate::fence::{self, Fence};
use crate::gpu::GpuDevice;
use crate::uapi::*;
use crate::wire::Wire;
use amdgpu_fmt::pm4;
use linux::ab::errno_more::ECANCELED;
use linux::errno::*;
use std::sync::Arc;
use std::vec;
use std::vec::Vec;

pub(crate) const GTT: u64 = AMDGPU_GEM_DOMAIN_GTT as u64;
pub(crate) const RW: u32 = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE | AMDGPU_VM_PAGE_EXECUTABLE;
/// Where the test maps its object: the IB at +0, results at +0x800.
pub(crate) const V: u64 = 0x1_0000_0000;
pub(crate) const OUT: u64 = V + 0x800;
const NICE: Caller = Caller { pid: 42, sys_admin: false, sys_nice: true };

pub(crate) fn bytes<T: Wire>(v: &T) -> Vec<u8> {
    let mut b = vec![0u8; T::SIZE];
    v.encode(&mut b).unwrap();
    b
}

pub(crate) fn put_bytes(fx: &Fx, off: usize, b: &[u8]) {
    use vfs::sys::UserMem;
    assert!(fx.m.write(BASE + off as u64, b));
}

pub(crate) fn ctx_alloc(fx: &Fx, k: usize, prio: i32, who: Caller) -> Result<u32, vfs::Errno> {
    let mut a = drm_amdgpu_ctx::ZERO;
    a.set_in(&drm_amdgpu_ctx_in { op: AMDGPU_CTX_OP_ALLOC_CTX, flags: 0, ctx_id: 0, priority: prio });
    fx.m.put(0, &a);
    fx.f(k).ioctl_with(DRM_IOCTL_AMDGPU_CTX, BASE, &fx.m, who, Some(fx.fds.clone()))?;
    Ok(fx.m.take::<drm_amdgpu_ctx>(0).bytes[0..4].iter().rev().fold(0u32, |a, b| (a << 8) | *b as u32))
}

pub(crate) fn query2(fx: &Fx, id: u32) -> u64 {
    let mut a = drm_amdgpu_ctx::ZERO;
    a.set_in(&drm_amdgpu_ctx_in { op: AMDGPU_CTX_OP_QUERY_STATE2, flags: 0, ctx_id: id, priority: 0 });
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_CTX).unwrap();
    u64::from_le_bytes(fx.m.take::<drm_amdgpu_ctx>(0).bytes[0..8].try_into().unwrap())
}

pub(crate) fn map(fx: &Fx, k: usize, h: u32, addr: u64, size: u64, flags: u32) {
    let a = drm_amdgpu_gem_va { handle: h, operation: AMDGPU_VA_OP_MAP, flags, va_address: addr, offset_in_bo: 0, map_size: size, ..drm_amdgpu_gem_va::ZERO };
    fx.m.put(0, &a);
    fx.io(k, DRM_IOCTL_AMDGPU_GEM_VA).unwrap();
}

pub(crate) fn syncobj(fx: &Fx, k: usize) -> u32 {
    fx.m.put(0, &drm_syncobj_create { handle: 0, flags: 0 });
    fx.io(k, DRM_IOCTL_SYNCOBJ_CREATE).unwrap();
    fx.m.take::<drm_syncobj_create>(0).handle
}

pub(crate) fn ib(ip: u32, ring: u32, va: u64, dw: u32, flags: u32) -> (u32, Vec<u8>) {
    (AMDGPU_CHUNK_ID_IB, bytes(&drm_amdgpu_cs_chunk_ib { _pad: 0, flags, va_start: va, ib_bytes: dw * 4, ip_type: ip, ip_instance: 0, ring }))
}

pub(crate) fn gfx(dw: u32) -> (u32, Vec<u8>) {
    ib(AMDGPU_HW_IP_GFX, 0, V, dw, 0)
}

pub(crate) fn uf(h: u32, off: u32) -> (u32, Vec<u8>) {
    (AMDGPU_CHUNK_ID_FENCE, bytes(&drm_amdgpu_cs_chunk_fence { handle: h, offset: off }))
}

pub(crate) fn sem(id: u32, hs: &[u32]) -> (u32, Vec<u8>) {
    (id, hs.iter().flat_map(|h| h.to_le_bytes()).collect())
}

fn tl(id: u32, hs: &[(u32, u64, u32)]) -> (u32, Vec<u8>) {
    (id, hs.iter().flat_map(|&(handle, point, flags)| bytes(&drm_amdgpu_cs_chunk_syncobj { handle, flags, point })).collect())
}

fn dep(id: u32, ctx: u32, ip: u32, ring: u32, seq: u64) -> (u32, Vec<u8>) {
    (id, bytes(&drm_amdgpu_cs_chunk_dep { ip_type: ip, ip_instance: 0, ring, ctx_id: ctx, handle: seq }))
}

pub(crate) fn bo_handles(fx: &Fx, hs: &[u32]) -> (u32, Vec<u8>) {
    let list: Vec<u8> = hs.iter().flat_map(|&h| bytes(&drm_amdgpu_bo_list_entry { bo_handle: h, bo_priority: 0 })).collect();
    put_bytes(fx, 7168, &list);
    (AMDGPU_CHUNK_ID_BO_HANDLES, bytes(&drm_amdgpu_bo_list_in { operation: 0, list_handle: 0, bo_number: hs.len() as u32, bo_info_size: 8, bo_info_ptr: BASE + 7168 }))
}

/// A CS with `chunks` laid out in the test memory: args at 0, pointers at 64, headers, then the data.
pub(crate) fn cs_as(fx: &Fx, k: usize, ctx_id: u32, bo_list: u32, chunks: &[(u32, Vec<u8>)]) -> Result<u64, vfs::Errno> {
    let n = chunks.len();
    let (ptrs, mut hdr, mut data) = (64usize, 64 + 8 * n, 64 + 24 * n);
    for (i, (id, d)) in chunks.iter().enumerate() {
        put_bytes(fx, ptrs + 8 * i, &(BASE + hdr as u64).to_le_bytes());
        put_bytes(fx, hdr, &bytes(&drm_amdgpu_cs_chunk { chunk_id: *id, length_dw: d.len() as u32 / 4, chunk_data: BASE + data as u64 }));
        put_bytes(fx, data, d);
        hdr += 16;
        data += d.len().div_ceil(8) * 8;
        assert!(data <= 7168, "test memory");
    }
    let mut a = drm_amdgpu_cs::ZERO;
    a.set_in(&drm_amdgpu_cs_in { ctx_id, bo_list_handle: bo_list, num_chunks: n as u32, flags: 0, chunks: BASE + ptrs as u64 });
    fx.m.put(0, &a);
    fx.io(k, DRM_IOCTL_AMDGPU_CS)?;
    Ok(fx.m.take::<drm_amdgpu_cs>(0).out().handle)
}

pub(crate) fn cs(fx: &Fx, ctx_id: u32, chunks: &[(u32, Vec<u8>)]) -> Result<u64, vfs::Errno> {
    cs_as(fx, 0, ctx_id, 0, chunks)
}

pub(crate) fn wait_cs(fx: &Fx, ctx_id: u32, ip: u32, ring: u32, seq: u64, timeout: u64) -> Result<u64, vfs::Errno> {
    let mut a = drm_amdgpu_wait_cs::ZERO;
    a.set_in(&drm_amdgpu_wait_cs_in { handle: seq, timeout, ip_type: ip, ip_instance: 0, ring, ctx_id });
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_WAIT_CS)?;
    Ok(fx.m.take::<drm_amdgpu_wait_cs>(0).out().status)
}

fn wait_fences(fx: &Fx, fs: &[(u32, u64)], all: bool, timeout: u64) -> Result<(u32, u32), vfs::Errno> {
    let list: Vec<u8> = fs.iter().flat_map(|&(c, s)| bytes(&drm_amdgpu_fence { ctx_id: c, ip_type: 0, ip_instance: 0, ring: 0, seq_no: s })).collect();
    put_bytes(fx, 1024, &list);
    let mut a = drm_amdgpu_wait_fences::ZERO;
    a.set_in(&drm_amdgpu_wait_fences_in { fences: BASE + 1024, fence_count: fs.len() as u32, wait_all: all as u32, timeout_ns: timeout });
    fx.m.put(0, &a);
    let r = fx.io(0, DRM_IOCTL_AMDGPU_WAIT_FENCES);
    let o = fx.m.take::<drm_amdgpu_wait_fences>(0).out();
    r.map(|_| (o.status, o.first_signaled))
}

/// The IB: WRITE_DATA of `vals` at OUT, then a 64-bit RELEASE_MEM of `seq` at OUT + 0x40.
pub(crate) fn program(vals: &[u32], seq: u64) -> Vec<u32> {
    let mut p = Vec::new();
    pm4::write_data(&mut p, pm4::dst_sel::MEM, OUT, 0, vals).unwrap();
    pm4::fence(&mut p, OUT + 0x40, seq, true, false).unwrap();
    p
}

/// An object of 2 pages mapped at V holding `ib` at its start (returns handle, dwords).
pub(crate) fn setup(fx: &Fx, k: usize, ib: &[u32]) -> (u32, u32) {
    let h = fx.create_in(k, 8192, 0, GTT, 0).unwrap();
    map(fx, k, h, V, 8192, RW);
    let b: Vec<u8> = ib.iter().flat_map(|d| d.to_le_bytes()).collect();
    assert!(fx.f(k).drm_file().bo(h).unwrap().write(0, &b));
    (h, ib.len() as u32)
}

pub(crate) fn read(fx: &Fx, h: u32, off: u64, n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    assert!(fx.bo(0, h).read(off, &mut b));
    b
}

pub(crate) fn set_syncobj(fx: &Fx, h: u32, f: &Arc<Fence>) {
    fx.f(0).drm_file().syncobj(h).unwrap().replace_fence(Some(f.clone()));
}

#[test]
fn dr8_cs_runs_and_writes_memory() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[0x1122_3344, 0x5566_7788], 0xdead_beef_0000_0001));
    let fbo = fx.create(4096, GTT, 0).unwrap();
    assert_eq!(cs(&fx, c, &[gfx(dw), uf(fbo, 16)]), Ok(1));
    assert_eq!(read(&fx, h, 0x800, 8), [0x44, 0x33, 0x22, 0x11, 0x88, 0x77, 0x66, 0x55]);
    assert_eq!(read(&fx, h, 0x840, 8), 0xdead_beef_0000_0001u64.to_le_bytes());
    assert_eq!(read(&fx, fbo, 16, 8), 1u64.to_le_bytes(), "user fence = sequence number");
    assert_eq!(read(&fx, fbo, 24, 8), [0; 8], "nothing beyond the 8 bytes");
    // The out handle replaces the first 8 bytes only (Linux leaves the rest of the union).
    assert_eq!(fx.m.take::<drm_amdgpu_cs>(0).r#in().num_chunks, 2);
    assert_eq!(cs(&fx, c, &[gfx(dw), uf(fbo, 16)]), Ok(2));
    assert_eq!(read(&fx, fbo, 16, 8), 2u64.to_le_bytes());
    assert_eq!(wait_cs(&fx, c, 0, 0, 2, 0), Ok(0));
    assert_eq!(wait_cs(&fx, c, 0, 0, u64::MAX, 0), Ok(0), "the last job");
    assert_eq!(wait_cs(&fx, c, 0, 0, 3, 0), Err(EINVAL), "not handed out");
    // A second context and the compute entity count on their own.
    let c2 = ctx_alloc(&fx, 0, 0, ME).unwrap();
    assert_eq!(cs(&fx, c2, &[gfx(dw)]), Ok(1));
    assert_eq!(cs(&fx, c2, &[ib(AMDGPU_HW_IP_COMPUTE, 3, V, dw, 0)]), Ok(1));
    // The object got the job's fence as the writer (implicit sync for the next user).
    assert!(!fx.bo(0, fbo).resv().is_empty());
    assert_eq!(fx.gpu.cs_stats().1, 0, "no hang");
}

#[test]
fn dr8_cs_argument_errors_in_linux_order() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (_h, dw) = setup(&fx, 0, &program(&[1], 1));
    let fbo = fx.create(4096, GTT, 0).unwrap();
    let big = fx.create(8192, GTT, 0).unwrap();
    let s = syncobj(&fx, 0);
    // parser_init
    assert_eq!(cs(&fx, c, &[]), Err(EINVAL), "no chunks");
    assert_eq!(cs(&fx, c + 7, &[gfx(dw)]), Err(EINVAL), "no such context");
    // pass1: copy-in faults
    let mut a = drm_amdgpu_cs::ZERO;
    a.set_in(&drm_amdgpu_cs_in { ctx_id: c, bo_list_handle: 0, num_chunks: 1, flags: 0, chunks: 8 });
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_CS), Err(EFAULT), "chunk array");
    put_bytes(&fx, 64, &8u64.to_le_bytes());
    a.set_in(&drm_amdgpu_cs_in { ctx_id: c, bo_list_handle: 0, num_chunks: 1, flags: 0, chunks: BASE + 64 });
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_CS), Err(EFAULT), "chunk header");
    put_bytes(&fx, 64, &(BASE + 128).to_le_bytes());
    put_bytes(&fx, 128, &bytes(&drm_amdgpu_cs_chunk { chunk_id: AMDGPU_CHUNK_ID_IB, length_dw: 8, chunk_data: 8 }));
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_CS), Err(EFAULT), "chunk data");
    a.set_in(&drm_amdgpu_cs_in { ctx_id: c, bo_list_handle: 0, num_chunks: 0x1000_0000, flags: 0, chunks: BASE + 64 });
    fx.m.put(0, &a);
    assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_CS), Err(ENOMEM), "above kvmalloc's limit");
    // pass1: chunk checks
    assert_eq!(cs(&fx, c, &[(11, vec![0; 8])]), Err(EINVAL), "unknown chunk id");
    assert_eq!(cs(&fx, c, &[(AMDGPU_CHUNK_ID_IB, vec![0; 28])]), Err(EINVAL), "short IB chunk");
    assert_eq!(cs(&fx, c, &[ib(AMDGPU_HW_IP_NUM, 0, V, dw, 0)]), Err(EINVAL), "IP type");
    assert_eq!(cs(&fx, c, &[ib(AMDGPU_HW_IP_GFX, 1, V, dw, 0)]), Err(EINVAL), "GFX has one entity");
    assert_eq!(cs(&fx, c, &[ib(AMDGPU_HW_IP_VCN_DEC, 0, V, dw, 0)]), Err(EINVAL), "no ring ready");
    let mut inst = ib(0, 0, V, dw, 0);
    inst.1[24] = 1;
    assert_eq!(cs(&fx, c, &[inst]), Err(EINVAL), "instance");
    assert_eq!(cs(&fx, c, &[uf(fbo, 0)]), Err(EINVAL), "no IB");
    assert_eq!(cs(&fx, c, &[gfx(dw), (AMDGPU_CHUNK_ID_FENCE, vec![0; 4])]), Err(EINVAL), "short fence chunk");
    assert_eq!(cs(&fx, c, &[gfx(dw), uf(999, 0)]), Err(EINVAL), "fence object handle (EINVAL, not ENOENT)");
    assert_eq!(cs(&fx, c, &[gfx(dw), uf(big, 0)]), Err(EINVAL), "fence object not one page");
    assert_eq!(cs(&fx, c, &[gfx(dw), uf(fbo, 4089)]), Err(EINVAL), "fence past the page");
    // amdgpu_cs_p1_user_fence has no alignment rule: offset 4 is taken, the 64-bit write lands there.
    let seq = cs(&fx, c, &[gfx(dw), uf(fbo, 4)]).unwrap();
    assert_eq!(read(&fx, fbo, 4, 8), seq.to_le_bytes(), "unaligned user fence");
    assert_eq!(cs(&fx, c, &[gfx(dw), (AMDGPU_CHUNK_ID_BO_HANDLES, vec![0; 20])]), Err(EINVAL), "short BO list");
    assert_eq!(cs(&fx, c, &[gfx(dw), bo_handles(&fx, &[fbo, 777])]), Err(ENOENT), "BO list handle");
    assert_eq!(cs(&fx, c, &[gfx(dw), bo_handles(&fx, &[fbo]), bo_handles(&fx, &[fbo])]), Err(EINVAL), "two BO lists (CachyOS)");
    assert_eq!(cs(&fx, c, &[gfx(dw), uf(fbo, 0), uf(fbo, 8)]), Err(EINVAL), "two user fences (CachyOS)");
    let mut sized = bo_handles(&fx, &[fbo]);
    sized.1[12] = 4;
    assert_eq!(cs(&fx, c, &[gfx(dw), sized]), Err(EINVAL), "BO list entry size 4 (CachyOS)");
    let short = (AMDGPU_CHUNK_ID_CP_GFX_SHADOW, vec![0u8; 8]);
    assert_eq!(cs(&fx, c, &[gfx(dw), short, bo_handles(&fx, &[777])]), Err(EINVAL), "short shadow chunk, in pass 1 (CachyOS)");
    let gang: Vec<_> = (0..4).map(|r| ib(AMDGPU_HW_IP_COMPUTE, r, V, dw, 0)).chain([gfx(dw)]).collect();
    assert_eq!(cs(&fx, c, &gang), Err(EINVAL), "five entities");
    // pass2, in chunk order
    let p = AMDGPU_IB_FLAG_PREEMPT;
    assert_eq!(cs(&fx, c, &[ib(0, 0, V, dw, p), ib(0, 0, V, dw, p)]), Err(EINVAL), "two preemptible DE IBs");
    assert_eq!(cs(&fx, c, &[ib(0, 0, V, dw, p), ib(0, 0, V, dw, p | AMDGPU_IB_FLAG_CE)]), Err(EINVAL), "CE (CachyOS: no CE flag at all)");
    assert_eq!(cs(&fx, c, &[ib(AMDGPU_HW_IP_COMPUTE, 0, V, dw, AMDGPU_IB_FLAG_CE)]), Err(EINVAL), "CE on compute");
    assert_eq!(cs(&fx, c, &[ib(0, 0, V, dw, AMDGPU_IB_FLAG_CE), bo_handles(&fx, &[777])]), Err(ENOENT), "CE is a pass-2 check");
    assert_eq!(cs(&fx, c, &[ib(0, 0, V, dw, p)]).map(|_| ()), Ok(()), "one preemptible IB");
    assert_eq!(cs(&fx, c, &[gfx(dw), dep(AMDGPU_CHUNK_ID_DEPENDENCIES, c, 0, 0, 99)]), Err(EINVAL), "dependency not handed out");
    assert_eq!(cs(&fx, c, &[gfx(dw), dep(AMDGPU_CHUNK_ID_DEPENDENCIES, c + 9, 0, 0, 1)]), Err(EINVAL), "dependency context");
    assert_eq!(cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[555])]), Err(ENOENT), "in-syncobj handle");
    assert_eq!(cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[s])]), Err(EINVAL), "in-syncobj without fence");
    assert_eq!(cs(&fx, c, &[gfx(dw), tl(AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_WAIT, &[(s, 3, 0)])]), Err(EINVAL), "point without fence");
    assert_eq!(cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[555])]), Err(EINVAL), "out-syncobj handle (EINVAL)");
    let two = [gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[]), tl(AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_SIGNAL, &[(s, 1, 0)])];
    assert_eq!(cs(&fx, c, &two), Err(EINVAL), "a second out chunk, even after an empty one");
    let shadow = |f: u64| (AMDGPU_CHUNK_ID_CP_GFX_SHADOW, bytes(&drm_amdgpu_cs_chunk_cp_gfx_shadow { shadow_va: 0, csa_va: 0, gds_va: 0, flags: f }));
    assert_eq!(cs(&fx, c, &[gfx(dw), shadow(2)]), Err(EINVAL), "shadow flags");
    assert_eq!(cs(&fx, c, &[gfx(dw), shadow(1)]).map(|_| ()), Ok(()), "INIT_SHADOW");
    // parser_bos: BO_LIST objects (DR-9) do not exist yet; not together with a BO_HANDLES chunk.
    assert_eq!(cs_as(&fx, 0, c, 5, &[gfx(dw)]), Err(ENOENT), "BO list object");
    assert_eq!(cs_as(&fx, 0, c, 5, &[gfx(dw), bo_handles(&fx, &[fbo])]), Err(EINVAL), "both lists");
    // An error leaves no sequence number behind and nothing on the objects.
    let before = wait_cs(&fx, c, 0, 0, u64::MAX, 0);
    assert_eq!(before, Ok(0));
    assert_eq!(fx.gpu.cs_stats().1, 0);
}

#[test]
fn dr8_ib_limits_per_ip() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (_h, dw) = setup(&fx, 0, &program(&[1], 1));
    // All chunk pointers name one header: 192 GFX IBs pass, 193 do not (amdgpu_ring_max_ibs).
    for (n, want) in [(192u32, true), (193, false)] {
        put_bytes(&fx, 4096, &bytes(&drm_amdgpu_cs_chunk { chunk_id: AMDGPU_CHUNK_ID_IB, length_dw: 8, chunk_data: BASE + 4096 + 16 }));
        put_bytes(&fx, 4096 + 16, &gfx(dw).1);
        for i in 0..n as usize {
            put_bytes(&fx, 1024 + 8 * i, &(BASE + 4096).to_le_bytes());
        }
        let mut a = drm_amdgpu_cs::ZERO;
        a.set_in(&drm_amdgpu_cs_in { ctx_id: c, bo_list_handle: 0, num_chunks: n, flags: 0, chunks: BASE + 1024 });
        fx.m.put(0, &a);
        assert_eq!(fx.io(0, DRM_IOCTL_AMDGPU_CS).is_ok(), want, "{n} IBs");
    }
}

#[test]
fn dr8_dependencies_and_syncobjs() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[7], 7));
    let (sin, sout, stl) = (syncobj(&fx, 0), syncobj(&fx, 0), syncobj(&fx, 0));
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    let seq = cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[sout])]).unwrap();
    assert_eq!(read(&fx, h, 0x800, 4), [0; 4], "waits for its in-syncobj");
    assert_eq!(wait_cs(&fx, c, 0, 0, seq, 0), Ok(1), "busy at a past deadline");
    let soon = fence::now().unwrap() + 20_000_000;
    assert_eq!(wait_cs(&fx, c, 0, 0, seq, soon), Ok(1), "busy at a near deadline");
    assert!(fence::now().unwrap() >= soon);
    // A dependency on that job by sequence number, and a timeline point out.
    let p2 = program(&[8], 8);
    let b: Vec<u8> = p2.iter().flat_map(|d| d.to_le_bytes()).collect();
    fx.bo(0, h).write(0x400, &b);
    let seq2 = cs(&fx, c, &[ib(0, 0, V + 0x400, p2.len() as u32, 0), dep(AMDGPU_CHUNK_ID_DEPENDENCIES, c, 0, 0, seq), tl(AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_SIGNAL, &[(stl, 5, 0)])]).unwrap();
    assert_eq!(seq2, seq + 1);
    let out = fx.f(0).drm_file().syncobj(sout).unwrap().fence().unwrap();
    assert!(!fence::is_signaled(&out));
    assert_eq!(wait_fences(&fx, &[(c, seq), (c, seq2)], false, 0), Ok((0, u32::MAX)), "none done");
    fx.gpu.ring().signal_all();
    assert!(fence::is_signaled(&out), "the out-syncobj has the job's fence");
    assert_eq!(read(&fx, h, 0x800, 4), 8u32.to_le_bytes(), "both ran, in order");
    assert_eq!(wait_fences(&fx, &[(c, seq), (c, seq2)], true, 0), Ok((1, 0)));
    assert_eq!(wait_fences(&fx, &[(c, seq2), (c, seq)], false, 0), Ok((1, 0)));
    assert_eq!(wait_fences(&fx, &[], false, 0), Err(EINVAL), "wait for any of none");
    assert_eq!(wait_fences(&fx, &[], true, 0), Err(EINVAL), "CachyOS: none at all");
    assert_eq!(wait_fences(&fx, &[(c, 99)], true, 0), Err(EINVAL));
    // The timeline point 5 waits like any point.
    let seq3 = cs(&fx, c, &[gfx(dw), tl(AMDGPU_CHUNK_ID_SYNCOBJ_TIMELINE_WAIT, &[(stl, 5, 0)]), dep(AMDGPU_CHUNK_ID_SCHEDULED_DEPENDENCIES, c, 0, 0, seq2)]).unwrap();
    assert_eq!(wait_cs(&fx, c, 0, 0, seq3, 0), Ok(0));
}

#[test]
fn dr8_implicit_sync_and_explicit_objects() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[9], 9));
    let other = fx.create(4096, GTT, 0).unwrap();
    let ex = fx.create(4096, GTT, AMDGPU_GEM_CREATE_EXPLICIT_SYNC as u64).unwrap();
    let busy = fx.gpu.ring().emit();
    fx.bo(0, other).fence(&busy, true).unwrap();
    fx.bo(0, ex).fence(&busy, true).unwrap();
    let seq = cs(&fx, c, &[gfx(dw), bo_handles(&fx, &[ex])]).unwrap();
    assert_eq!(wait_cs(&fx, c, 0, 0, seq, 0), Ok(0), "an EXPLICIT_SYNC object does not wait for readers");
    let seq = cs(&fx, c, &[gfx(dw), bo_handles(&fx, &[other, h])]).unwrap();
    assert_eq!(wait_cs(&fx, c, 0, 0, seq, 0), Ok(1), "waits for the reader of a listed object");
    fx.gpu.ring().signal_all();
    assert_eq!(wait_cs(&fx, c, 0, 0, seq, 0), Ok(0));
}

#[test]
fn dr8_priority_orders_ready_jobs() {
    let fx = Fx::new();
    assert_eq!(ctx_alloc(&fx, 0, AMDGPU_CTX_PRIORITY_HIGH as i32, ME), Err(EACCES));
    let lo = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let hi = ctx_alloc(&fx, 0, AMDGPU_CTX_PRIORITY_HIGH as i32, NICE).unwrap();
    let (h, _) = setup(&fx, 0, &[]);
    let (a, b) = (program(&[0xaaaa], 1), program(&[0xbbbb], 2));
    fx.bo(0, h).write(0, &a.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>());
    fx.bo(0, h).write(0x400, &b.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>());
    let s = syncobj(&fx, 0);
    set_syncobj(&fx, s, &fx.gpu.ring().emit());
    cs(&fx, lo, &[ib(0, 0, V, a.len() as u32, 0), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[s])]).unwrap();
    cs(&fx, hi, &[ib(0, 0, V + 0x400, b.len() as u32, 0), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[s])]).unwrap();
    fx.gpu.ring().signal_all();
    assert_eq!(read(&fx, h, 0x800, 4), 0xaaaau32.to_le_bytes(), "HIGH ran first, the NORMAL job wrote last");
}

#[test]
fn dr8_compute_refuses_a_nested_ib_without_its_upper_bits() {
    const HIGH: u64 = 0xffff_8000_0010_0000;
    let run = |ip: u32, hi: u32| {
        let fx = Fx::new();
        let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
        let inner = fx.create_in(0, 4096, 0, GTT, 0).unwrap();
        map(&fx, 0, inner, HIGH, 4096, RW);
        let mut p = Vec::new();
        pm4::write_data(&mut p, pm4::dst_sel::MEM, OUT, 0, &[0xabcd]).unwrap();
        let b: Vec<u8> = p.iter().flat_map(|d| d.to_le_bytes()).collect();
        assert!(fx.f(0).drm_file().bo(inner).unwrap().write(0, &b));
        let (h, dw) = setup(&fx, 0, &[pm4::packet3(pm4::op::INDIRECT_BUFFER, 2), HIGH as u32, hi, p.len() as u32]);
        cs(&fx, c, &[ib(ip, 0, V, dw, 0)]).unwrap();
        (fx.gpu.cs_stats().1, read(&fx, h, 0x800, 4) == 0xabcdu32.to_le_bytes())
    };
    assert_eq!(run(AMDGPU_HW_IP_COMPUTE, 0xffff_8000), (0, true), "sign-extended, as RADV writes it");
    assert_eq!(run(AMDGPU_HW_IP_COMPUTE, 0x8000), (1, false), "no upper bits: CP_HQD_ERROR.SUA_ERROR on the MEC");
    assert_eq!(run(AMDGPU_HW_IP_GFX, 0x8000), (0, true), "the GFX queue takes both");
}

#[test]
fn dr8_hang_makes_the_context_guilty() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let inno = ctx_alloc(&fx, 0, 0, ME).unwrap();
    // An unknown PM4 opcode halts the CP.
    let (h, _) = setup(&fx, 0, &[0xc000_ff00, 0]);
    let s = syncobj(&fx, 0);
    let seq = cs(&fx, c, &[ib(0, 0, V, 2, 0), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[s])]).unwrap();
    let f = fx.f(0).drm_file().syncobj(s).unwrap().fence().unwrap();
    assert_eq!(fence::status(&f), -(ETIME as i32), "Linux's job timeout");
    assert_eq!(fx.gpu.cs_stats().1, 1);
    assert_eq!(query2(&fx, c), (AMDGPU_CTX_QUERY2_FLAGS_RESET | AMDGPU_CTX_QUERY2_FLAGS_GUILTY) as u64);
    assert_eq!(query2(&fx, inno), AMDGPU_CTX_QUERY2_FLAGS_RESET as u64, "an innocent context sees the reset");
    assert_eq!(cs(&fx, c, &[ib(0, 0, V, 2, 0)]), Err(ECANCELED), "a guilty context submits no more");
    assert_eq!(wait_cs(&fx, c, 0, 0, seq, 0), Err(ETIME), "the entity's error");
    // A fault: the IB outside every mapping, then a write to a read-only page.
    let c3 = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let seq = cs(&fx, c3, &[ib(0, 0, V + 0x10_0000, 4, 0)]).unwrap();
    assert_eq!(wait_cs(&fx, c3, 0, 0, seq, 0), Err(ETIME));
    let ro = fx.create(4096, GTT, 0).unwrap();
    map(&fx, 0, ro, V + 0x10_0000, 4096, AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_EXECUTABLE);
    let mut p = Vec::new();
    pm4::write_data(&mut p, pm4::dst_sel::MEM, V + 0x10_0000 + 0x100, 0, &[1]).unwrap();
    fx.bo(0, ro).write(0, &p.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>());
    let c4 = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let seq = cs(&fx, c4, &[ib(0, 0, V + 0x10_0000, p.len() as u32, 0)]).unwrap();
    assert_eq!(wait_cs(&fx, c4, 0, 0, seq, 0), Err(ETIME), "write to a read-only page");
    assert_eq!(read(&fx, ro, 0x100, 4), [0; 4]);
    let _ = h;
}

/// WY-21: `drm_fake_hang=lose`: the hang ends the device as an unrecoverable one on the chip: no new file, no submission.
#[test]
fn dr8_a_hang_can_end_the_fake_device() {
    let fx = Fx::new();
    fx.gpu.lose_on_hang(true);
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    // An unknown PM4 opcode halts the CP.
    let (h, _) = setup(&fx, 0, &[0xc000_ff00, 0]);
    assert!(!fx.gpu.lost());
    let seq = cs(&fx, c, &[ib(0, 0, V, 2, 0)]).unwrap();
    assert_eq!(wait_cs(&fx, c, 0, 0, seq, 0), Err(ETIME));
    assert!(fx.gpu.lost());
    let fresh = ctx_alloc(&fx, 0, 0, ME).unwrap();
    assert_eq!(cs(&fx, fresh, &[ib(0, 0, V, 2, 0)]), Err(ENODEV), "a lost device takes no job");
    let opened = crate::node::open_device(fx.dev.clone(), crate::file::MinorKind::Render, false).map(|_| ());
    assert_eq!(opened, Err(ENODEV), "a lost device takes no new file");
    let _ = h;
}

#[test]
fn dr8_vram_lost_cancels() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (_h, dw) = setup(&fx, 0, &program(&[1], 1));
    let s = syncobj(&fx, 0);
    let (sin, gate) = (syncobj(&fx, 0), fx.gpu.ring().emit());
    set_syncobj(&fx, sin, &gate);
    cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[s])]).unwrap();
    fx.gpu.reset(true);
    fx.gpu.ring().signal_all();
    let f = fx.f(0).drm_file().syncobj(s).unwrap().fence().unwrap();
    assert_eq!(fence::status(&f), -(ECANCELED as i32), "a job of a lost generation does not run");
    assert_eq!(cs(&fx, c, &[gfx(dw)]), Err(ECANCELED));
    assert_eq!(query2(&fx, c) & AMDGPU_CTX_QUERY2_FLAGS_VRAMLOST as u64, AMDGPU_CTX_QUERY2_FLAGS_VRAMLOST as u64);
}

#[test]
fn dr8_gang_runs_together() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, _) = setup(&fx, 0, &[]);
    let (a, b) = (program(&[0x11], 1), program(&[0x22], 2));
    fx.bo(0, h).write(0, &a.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>());
    fx.bo(0, h).write(0x400, &b.iter().flat_map(|d| d.to_le_bytes()).collect::<Vec<u8>>());
    // The last IB's entity leads: its sequence number comes back, the member's entity gets none.
    let seq = cs(&fx, c, &[ib(AMDGPU_HW_IP_COMPUTE, 0, V, a.len() as u32, 0), ib(0, 0, V + 0x400, b.len() as u32, 0)]).unwrap();
    assert_eq!(seq, 1);
    assert_eq!(fx.gpu.cs_stats().0, 2, "two jobs");
    assert_eq!(wait_cs(&fx, c, AMDGPU_HW_IP_COMPUTE, 0, 1, 0), Err(EINVAL), "no number on the member's entity");
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, 0), Ok(0));
    assert_eq!(read(&fx, h, 0x800, 4), 0x22u32.to_le_bytes());
}

#[test]
fn dr8_jobs_outlive_their_file() {
    let mut fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[0x77], 1));
    let (sin, sout) = (syncobj(&fx, 0), syncobj(&fx, 0));
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[sout])]).unwrap();
    let out = fx.f(0).drm_file().syncobj(sout).unwrap().fence().unwrap();
    let bo = fx.bo(0, h);
    // The dependency signals while the last close waits (its second): the job runs to its end.
    let g = gate.clone();
    let t = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100));
        g.signal();
    });
    fx.files[0] = None;
    t.join().unwrap();
    assert_eq!(fence::status(&out), 1);
    let mut b = [0u8; 4];
    assert!(bo.read(0x800, &mut b));
    assert_eq!(b, 0x77u32.to_le_bytes(), "the job ran after its file went");
    drop(bo);

    // A job whose dependency never comes in time ends with ESRCH, and holds its objects until then.
    let c = ctx_alloc(&fx, 1, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 1, &program(&[0x99], 1));
    let (sin, sout) = (syncobj(&fx, 1), syncobj(&fx, 1));
    let gate = fx.gpu.ring().emit();
    fx.f(1).drm_file().syncobj(sin).unwrap().replace_fence(Some(gate.clone()));
    let list = bo_handles(&fx, &[h]);
    cs_as(&fx, 1, c, 0, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[sout]), list]).unwrap();
    let out = fx.f(1).drm_file().syncobj(sout).unwrap().fence().unwrap();
    let weak = Arc::downgrade(&fx.f(1).drm_file().bo(h).unwrap());
    fx.files[1] = None;
    assert!(weak.upgrade().is_some(), "the waiting job holds its listed object");
    assert!(fx.gpu.bo_usage().1 > 0, "and the VM's memory");
    assert!(!fence::is_signaled(&out));
    assert_eq!(fx.gpu.cs_stats().2, 1);
    gate.signal();
    assert_eq!(fence::status(&out), -(ESRCH as i32), "drm_sched_entity_kill");
    assert!(weak.upgrade().is_none(), "the object went with the job");
    assert_eq!(fx.gpu.bo_usage(), (0, 0));
    assert_eq!(fx.gpu.cs_stats().2, 0);
}

#[test]
fn dr8_free_ctx_and_dropped_device() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (_h, dw) = setup(&fx, 0, &program(&[1], 1));
    let (sin, sout) = (syncobj(&fx, 0), syncobj(&fx, 0));
    set_syncobj(&fx, sin, &fx.gpu.ring().emit());
    cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[sout])]).unwrap();
    let out = fx.f(0).drm_file().syncobj(sout).unwrap().fence().unwrap();
    let mut a = drm_amdgpu_ctx::ZERO;
    a.set_in(&drm_amdgpu_ctx_in { op: AMDGPU_CTX_OP_FREE_CTX, flags: 0, ctx_id: c, priority: 0 });
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_CTX).unwrap();
    assert_eq!(wait_cs(&fx, c, 0, 0, 1, 0), Err(EINVAL), "the context is gone");
    fx.gpu.ring().signal_all();
    assert_eq!(fence::status(&out), -(ESRCH as i32), "FREE ended the waiting job");
    assert!(fx.gpu.vram_lost_counter() == 0);
}

#[test]
fn dr8_implicit_sync_skips_the_files_own_jobs() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[5], 5));
    let sin = syncobj(&fx, 0);
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    let a = cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), bo_handles(&fx, &[h])]).unwrap();
    assert_eq!(wait_cs(&fx, c, 0, 0, a, 0), Ok(1), "A waits for its gate");
    // The same file on another ring: AMDGPU_SYNC_NE_OWNER (amdgpu_cs.c:1194) skips the file's own fences.
    let b = cs(&fx, c, &[ib(AMDGPU_HW_IP_COMPUTE, 0, V, dw, 0), bo_handles(&fx, &[h])]).unwrap();
    assert_eq!(wait_cs(&fx, c, AMDGPU_HW_IP_COMPUTE, 0, b, 0), Ok(0), "B does not wait for A's write");
    // Another file with the object (PRIME) waits for A.
    let fd = fx.to_fd(0, h, 0).unwrap();
    let h1 = fx.to_handle(1, fd).unwrap();
    let c1 = ctx_alloc(&fx, 1, 0, ME).unwrap();
    let (_i1, dw1) = setup(&fx, 1, &program(&[6], 6));
    let sout = syncobj(&fx, 1);
    cs_as(&fx, 1, c1, 0, &[gfx(dw1), bo_handles(&fx, &[h1]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[sout])]).unwrap();
    let out = fx.f(1).drm_file().syncobj(sout).unwrap().fence().unwrap();
    assert!(!fence::is_signaled(&out), "a foreign job waits for A's write");
    gate.signal();
    assert_eq!(fence::status(&out), 1);
    assert_eq!(wait_cs(&fx, c, 0, 0, a, 0), Ok(0));
}

#[test]
fn dr8_last_close_waits_once_for_all_contexts() {
    let mut fx = Fx::new();
    let (_h, dw) = setup(&fx, 1, &program(&[1], 1));
    let sin = syncobj(&fx, 1);
    let gate = fx.gpu.ring().emit();
    fx.f(1).drm_file().syncobj(sin).unwrap().replace_fence(Some(gate.clone()));
    let mut outs = Vec::new();
    for _ in 0..3 {
        let c = ctx_alloc(&fx, 1, 0, ME).unwrap();
        let so = syncobj(&fx, 1);
        cs_as(&fx, 1, c, 0, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[so])]).unwrap();
        outs.push(fx.f(1).drm_file().syncobj(so).unwrap().fence().unwrap());
    }
    let t = std::time::Instant::now();
    fx.files[1] = None;
    let took = t.elapsed();
    // Three contexts with a deadline each would take 3 s; one deadline for the close takes 1 s.
    assert!(took < std::time::Duration::from_millis(2500), "the close waited {took:?}");
    gate.signal();
    for o in outs {
        assert_eq!(fence::status(&o), -(ESRCH as i32));
    }
}

/// Finding 8 of DR-8, fixed in DR-9: Linux's page-table update of an UNMAP waits for the VM's pending jobs, so
/// a job queued before the unmap still sees its IB (the Clear goes to the fake behind the VM root's fences).
#[test]
fn dr8_unmap_waits_for_pending_jobs() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    let (h, dw) = setup(&fx, 0, &program(&[3], 3));
    let (sin, sout) = (syncobj(&fx, 0), syncobj(&fx, 0));
    let gate = fx.gpu.ring().emit();
    set_syncobj(&fx, sin, &gate);
    cs(&fx, c, &[gfx(dw), sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[sin]), sem(AMDGPU_CHUNK_ID_SYNCOBJ_OUT, &[sout])]).unwrap();
    let out = fx.f(0).drm_file().syncobj(sout).unwrap().fence().unwrap();
    let a = drm_amdgpu_gem_va { handle: h, operation: AMDGPU_VA_OP_UNMAP, flags: 0, va_address: V, offset_in_bo: 0, map_size: 8192, ..drm_amdgpu_gem_va::ZERO };
    fx.m.put(0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_GEM_VA).unwrap();
    gate.signal();
    assert_eq!(fence::status(&out), 1, "the job ran on the mapping it was submitted with");
}
