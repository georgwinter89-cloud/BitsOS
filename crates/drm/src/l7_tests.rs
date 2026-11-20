//! L7-GPU host tests: the answers Linux 7.3.1 gives where Linux 6.6 differed (plan/spec/linux7-differences.md
//! rows 10, 18, 19, 20, 25): GEM_CREATE's flags and domains, the new core numbers and the legacy holes,
//! SET_CLIENT_NAME and its fdinfo line, INFO UQ_FW_AREAS, the IB size per ring.

use crate::dr6_tests::{Fx, BASE};
use crate::dr8_tests::{ctx_alloc, cs, ib, put_bytes, sem, V};
use crate::dr6_tests::ME;
use crate::gem::AMDGPU_GEM_CREATE_GFX12_DCC;
use crate::gpu::GpuDevice;
use crate::uapi::*;
use linux::errno::*;
use std::string::String;
use std::{format, vec};
use std::vec::Vec;
use vfs::sys::UserMem;
use vfs::File;

const P: u64 = 4096;
const GTT: u64 = AMDGPU_GEM_DOMAIN_GTT as u64;
const VRAM: u64 = AMDGPU_GEM_DOMAIN_VRAM as u64;
const GDS: u64 = AMDGPU_GEM_DOMAIN_GDS as u64;
const GWS: u64 = AMDGPU_GEM_DOMAIN_GWS as u64;
const DOORBELL: u64 = AMDGPU_GEM_DOMAIN_DOORBELL as u64;
/// VRAM_CLEARED and VRAM_WIPE_ON_RELEASE: Linux 7.3.1 sets both on every user object.
const ALWAYS: u64 = (AMDGPU_GEM_CREATE_VRAM_CLEARED | AMDGPU_GEM_CREATE_VRAM_WIPE_ON_RELEASE) as u64;

#[test]
fn l7_gem_create_flags_and_domains() {
    let fx = Fx::new();
    // amdgpu_gem_are_domains_valid: a special domain comes alone.
    for d in [GDS | GTT, GDS | GWS, DOORBELL | GTT, DOORBELL | GDS, DOORBELL | VRAM | GTT] {
        assert_eq!(fx.create(P, d, 0), Err(EINVAL), "domains {d:#x}");
    }
    assert_eq!(fx.create(P, GDS | GTT, 1 << 30), Err(EINVAL));
    assert_eq!(fx.create(P, GTT, AMDGPU_GEM_CREATE_VRAM_CONTIGUOUS as u64), Err(EINVAL));
    // The settable mask of Linux 7.3.1 (amdgpu_gem.h:76-88).
    for f in [
        AMDGPU_GEM_CREATE_COHERENT as u64,
        AMDGPU_GEM_CREATE_UNCACHED as u64,
        AMDGPU_GEM_CREATE_EXT_COHERENT as u64,
        AMDGPU_GEM_CREATE_VRAM_WIPE_ON_RELEASE as u64,
        AMDGPU_GEM_CREATE_GFX12_DCC,
    ] {
        let h = fx.create(P, VRAM, f).unwrap();
        assert_eq!(fx.info(0, h).unwrap().domain_flags, f | ALWAYS, "flag {f:#x}");
    }
    // A doorbell object exists, has no pages, nothing maps it.
    let d = fx.create(P, DOORBELL, 0).unwrap();
    let i = fx.info(0, d).unwrap();
    assert_eq!((i.bo_size, i.alignment, i.domains, i.domain_flags), (P, 0, DOORBELL, ALWAYS));
    let bo = fx.bo(0, d);
    assert!(bo.place().is_none() && bo.memory().page_phys(0).is_none());
}

/// L7 row 10 (checker): DOORBELL objects share the doorbell BAR less the kernel's pages; past it ENOMEM, as
/// Linux's AMDGPU_PL_DOORBELL range manager (CachyOS: 1 GiB is ENOMEM).
#[test]
fn l7_doorbell_objects_share_the_bar() {
    let fx = Fx::new();
    let room = fx.gpu.profile().doorbell_room;
    assert_eq!(room, (2 << 20) - 2 * P);
    assert_eq!(fx.create(1 << 30, DOORBELL, 0), Err(ENOMEM));
    let a = fx.create(room - P, DOORBELL, 0).unwrap();
    assert_eq!(fx.create(2 * P, DOORBELL, 0), Err(ENOMEM), "only one page left");
    let b = fx.create(P, DOORBELL, 0).unwrap();
    assert_eq!(fx.create(P, DOORBELL, 0), Err(ENOMEM));
    fx.close(0, b).unwrap();
    fx.close(0, a).unwrap();
    // Closed objects give their share back.
    fx.create(room, DOORBELL, 0).unwrap();
}

#[test]
fn l7_uncached_objects_map_uncached() {
    let rw = AMDGPU_VM_PAGE_READABLE | AMDGPU_VM_PAGE_WRITEABLE;
    for f in [AMDGPU_GEM_CREATE_COHERENT, AMDGPU_GEM_CREATE_EXT_COHERENT, AMDGPU_GEM_CREATE_UNCACHED] {
        assert_eq!(crate::vm::pte_flags(f as u64, rw | AMDGPU_VM_MTYPE_NC), rw | AMDGPU_VM_MTYPE_UC, "{f:#x}");
    }
    assert_eq!(crate::vm::pte_flags(0, rw | AMDGPU_VM_MTYPE_NC), rw | AMDGPU_VM_MTYPE_NC);
    assert_eq!(crate::vm::pte_flags(AMDGPU_GEM_CREATE_CPU_GTT_USWC as u64, rw), rw);
}

/// One core ioctl with `arg` bytes at the test memory's start.
fn core(fx: &Fx, cmd: u32, arg: &[u8]) -> Result<i64, vfs::Errno> {
    put_bytes(fx, 0, arg);
    fx.io(0, cmd)
}

fn name_arg(len: u64, ptr: u64) -> Vec<u8> {
    [len.to_le_bytes(), ptr.to_le_bytes()].concat()
}

fn fdinfo(fx: &Fx) -> String {
    let mut s = String::new();
    fx.f(0).show_fdinfo(&mut s);
    s
}

#[test]
fn l7_core_table_and_client_name() {
    let fx = Fx::new();
    let any = [0u8; 16];
    // Legacy numbers are holes since Linux 6.8: EINVAL before the permission checks.
    for nr in [0x2au32, 0x20, 0x30, 0x08, 0x03, 0x04, 0x15, 0x38] {
        assert_eq!(core(&fx, crate::ioc::ioc(3, 0x64, nr, 8), &any), Err(EINVAL), "legacy {nr:#x}");
    }
    // drm_noop entries keep their flags: a render node may not call them.
    assert_eq!(core(&fx, crate::ioc::ioc(1, 0x64, 0x2c, 8), &any), Err(EACCES), "FINISH");
    assert_eq!(core(&fx, DRM_IOCTL_MODE_CLOSEFB, &any), Err(EACCES));
    assert_eq!(core(&fx, DRM_IOCTL_GEM_CHANGE_HANDLE, &[1, 0, 0, 0, 2, 0, 0, 0]), Err(EINVAL));
    assert_eq!(core(&fx, crate::ioc::ioc(3, 0x64, 0xd3, 8), &any), Err(EINVAL));

    assert!(!fdinfo(&fx).contains("drm-client-name"));
    put_bytes(&fx, 512, b"abi-test");
    assert_eq!(core(&fx, DRM_IOCTL_SET_CLIENT_NAME, &name_arg(8, BASE + 512)), Ok(0));
    let s = fdinfo(&fx);
    let pdev = s.find("drm-pdev:").unwrap();
    let at = s.find("drm-client-name:\tabi-test\n").unwrap();
    assert!(at > pdev, "after the PCI slot: {s}");
    put_bytes(&fx, 512, &[b'n'; 80]);
    assert_eq!(core(&fx, DRM_IOCTL_SET_CLIENT_NAME, &name_arg(64, BASE + 512)), Ok(0));
    let bad: [(&[u8], u64, vfs::Errno); 5] = [(b"nnnn", 65, EINVAL), (b"a b", 3, EINVAL), (b"a\tb", 3, EINVAL), (b"ab\0c", 4, EINVAL), (b"a\xe4", 2, EINVAL)];
    for (text, len, e) in bad {
        put_bytes(&fx, 512, text);
        assert_eq!(core(&fx, DRM_IOCTL_SET_CLIENT_NAME, &name_arg(len, BASE + 512)), Err(e), "{text:?}");
    }
    assert_eq!(core(&fx, DRM_IOCTL_SET_CLIENT_NAME, &name_arg(4, 8)), Err(EFAULT));
    assert!(fdinfo(&fx).contains(&format!("drm-client-name:\t{}\n", "n".repeat(64))), "a refused name keeps the old");
    assert_eq!(core(&fx, DRM_IOCTL_SET_CLIENT_NAME, &name_arg(0, 0)), Ok(0));
    assert!(!fdinfo(&fx).contains("drm-client-name"), "length 0 clears it");
    assert!(!fdinfo(&fx).is_empty() && fx.f(1).drm_file().client_name().is_none(), "per file");
}

/// AMDGPU_INFO with `ty` in the query's first word; the answer at +1024.
fn info(fx: &Fx, query: u32, ty: u32, size: u32, ptr: u64) -> Result<i64, vfs::Errno> {
    let mut a = Vec::new();
    a.extend_from_slice(&ptr.to_le_bytes());
    a.extend_from_slice(&size.to_le_bytes());
    a.extend_from_slice(&query.to_le_bytes());
    a.extend_from_slice(&ty.to_le_bytes());
    a.extend_from_slice(&[0; 12]);
    put_bytes(fx, 0, &a);
    fx.io(0, DRM_IOCTL_AMDGPU_INFO)
}

fn words(fx: &Fx, at: u64, n: usize) -> Vec<u32> {
    let mut b = vec![0u8; 4 * n];
    assert!(fx.m.read(BASE + at, &mut b));
    b.chunks(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect()
}

#[test]
fn l7_info_uq_fw_areas() {
    let fx = Fx::new();
    let want: [(u32, [u32; 4]); 3] = [
        (AMDGPU_HW_IP_GFX, [73728, 256, 484, 256]),
        (AMDGPU_HW_IP_COMPUTE, [2048, 256, 0, 0]),
        (AMDGPU_HW_IP_DMA, [32, 4, 0, 0]),
    ];
    for (ty, w) in want {
        put_bytes(&fx, 1024, &[0x5a; 20]);
        assert_eq!(info(&fx, AMDGPU_INFO_UQ_FW_AREAS, ty, 16, BASE + 1024), Ok(0), "type {ty}");
        assert_eq!(words(&fx, 1024, 5), [w[0], w[1], w[2], w[3], 0x5a5a_5a5a], "type {ty}");
    }
    put_bytes(&fx, 1024, &[0x5a; 8]);
    assert_eq!(info(&fx, AMDGPU_INFO_UQ_FW_AREAS, AMDGPU_HW_IP_GFX, 4, BASE + 1024), Ok(0));
    assert_eq!(words(&fx, 1024, 2), [73728, 0x5a5a_5a5a], "min(return_size, 16)");
    assert_eq!(info(&fx, AMDGPU_INFO_UQ_FW_AREAS, AMDGPU_HW_IP_UVD, 16, BASE + 1024), Err(EINVAL));
    assert_eq!(info(&fx, AMDGPU_INFO_UQ_FW_AREAS, 99, 16, BASE + 1024), Err(EINVAL));
    // Linux answers 0 although the copy faulted (amdgpu_kms.c:1398-1401).
    assert_eq!(info(&fx, AMDGPU_INFO_UQ_FW_AREAS, AMDGPU_HW_IP_GFX, 16, 8), Ok(0));
}

#[test]
fn l7_ib_size_fits_its_ring() {
    let fx = Fx::new();
    let c = ctx_alloc(&fx, 0, 0, ME).unwrap();
    // An unknown in-syncobj ends a CS whose IB passed with ENOENT (pass 2, after the IB).
    let unknown = sem(AMDGPU_CHUNK_ID_SYNCOBJ_IN, &[4444]);
    let cases = [
        (AMDGPU_HW_IP_GFX, 0xF_FFFF, ENOENT),
        (AMDGPU_HW_IP_GFX, 0x10_0000, EINVAL),
        (AMDGPU_HW_IP_COMPUTE, 0x10_0000, EINVAL),
        (AMDGPU_HW_IP_DMA, 0x10_0000, EINVAL),
        (AMDGPU_HW_IP_VCN_ENC, 0x10_0000, ENOENT),
        (AMDGPU_HW_IP_VCN_ENC, 0x7F_FFF0, ENOENT),
        (AMDGPU_HW_IP_VCN_ENC, 0x7F_FFF1, EINVAL),
    ];
    for (ip, dw, e) in cases {
        assert_eq!(cs(&fx, c, &[ib(ip, 0, V, dw, 0), unknown.clone()]), Err(e), "IP {ip}, {dw:#x} dwords");
    }
}

/// Row 57: 6.6's 16/40-byte and 7.3.1's 24/48-byte syncobj structs both work (the dispatcher zero-extends).
#[test]
fn l7_syncobj_sizes() {
    let fx = Fx::new();
    let h = crate::dr8_tests::syncobj(&fx, 0);
    for size in [16u32, 24] {
        let a: Vec<u8> = [h.to_le_bytes(), 0u32.to_le_bytes(), (-1i32).to_le_bytes(), 0u32.to_le_bytes(), [0; 4], [0; 4]].concat();
        assert_eq!(core(&fx, crate::ioc::ioc(3, 0x64, 0xc1, size), &a), Ok(0), "HANDLE_TO_FD of {size} bytes");
        let fd = words(&fx, 8, 1)[0] as i32;
        assert!(fd >= 0 && fx.fds.get(fd as usize).is_ok(), "{size}: fd {fd}");
    }
    put_bytes(&fx, 512, &h.to_le_bytes());
    put_bytes(&fx, 520, &0u64.to_le_bytes());
    for size in [40u32, 48] {
        let mut a = Vec::new();
        a.extend_from_slice(&(BASE + 512).to_le_bytes());
        a.extend_from_slice(&(BASE + 520).to_le_bytes());
        a.extend_from_slice(&0i64.to_le_bytes());
        a.extend_from_slice(&1u32.to_le_bytes());
        a.extend_from_slice(&DRM_SYNCOBJ_WAIT_FLAGS_WAIT_FOR_SUBMIT.to_le_bytes());
        a.extend_from_slice(&[0; 16]);
        // No fence yet and no time to wait: ETIME, from either size.
        assert_eq!(core(&fx, crate::ioc::ioc(3, 0x64, 0xca, size), &a[..size as usize]), Err(ETIME), "TIMELINE_WAIT of {size} bytes");
    }
}
