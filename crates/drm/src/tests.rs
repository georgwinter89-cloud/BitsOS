//! Spot checks against Linux and the scouts' traces, and the byte conversion.

use crate::Wire;
use crate::ioc;
use crate::uapi::*;

#[test]
fn known_ioctl_numbers() {
    // Values as strace prints them for Mesa on CachyOS (plan/hw/gx-scout-traces).
    assert_eq!(DRM_IOCTL_VERSION, 0xc040_6400);
    assert_eq!(DRM_IOCTL_GET_CAP, 0xc010_640c);
    assert_eq!(DRM_IOCTL_AMDGPU_GEM_CREATE, 0xc020_6440);
    assert_eq!(DRM_IOCTL_AMDGPU_INFO, 0x4020_6445);
    assert_eq!(DRM_IOCTL_AMDGPU_GEM_VA, 0x4040_6448);
    assert_eq!(SYNC_IOC_MERGE, 0xc030_3e03);
    assert_eq!(ioc::ty(DMA_BUF_SET_NAME_B), b'b' as u32);
    assert_eq!(ioc::nr(DRM_IOCTL_AMDGPU_CS), DRM_COMMAND_BASE + DRM_AMDGPU_CS);
}

#[test]
fn sizes_the_scouts_saw() {
    assert_eq!(<drm_amdgpu_gem_va as Wire>::SIZE, 64);
    assert_eq!(<drm_amdgpu_info as Wire>::SIZE, 32);
    assert_eq!(AMDGPU_CTX_PRIORITY_VERY_LOW, -1023);
}

#[test]
fn struct_round_trip_zeroes_padding() {
    let mut v = drm_version::default();
    v.version_major = 3;
    v.name_len = 6;
    v.name = 0x7fff_0000_1000;
    let mut b = [0xaau8; 70];
    v.encode(&mut b).unwrap();
    assert_eq!(&b[0..4], &3i32.to_le_bytes());
    assert_eq!(&b[12..16], &[0; 4]); // padding before name_len
    assert_eq!(&b[64..], &[0xaa; 6]); // nothing written past sizeof
    assert_eq!(drm_version::decode(&b), Some(v));
    assert_eq!(drm_version::decode(&b[..63]), None);
    assert_eq!(v.encode(&mut [0u8; 63]), None);
}

#[test]
fn union_accessors() {
    let mut u = drm_amdgpu_gem_create::default();
    let mut i = drm_amdgpu_gem_create_in::default();
    i.bo_size = 1 << 20;
    i.domains = AMDGPU_GEM_DOMAIN_GTT as u64;
    u.set_in(&i);
    assert_eq!(u.r#in(), i);
    let mut o = drm_amdgpu_gem_create_out::default();
    o.handle = 7;
    u.set_out(&o);
    assert_eq!(u.out().handle, 7);
    assert_eq!(&u.bytes[4..], &[0; 28]); // the old `in` bytes are gone
}

#[test]
fn anonymous_union_of_info() {
    let mut q = drm_amdgpu_info::default();
    q.query = AMDGPU_INFO_HW_IP_INFO;
    let mut hw = drm_amdgpu_info__query_hw_ip::default();
    hw.r#type = AMDGPU_HW_IP_GFX;
    q.anon0.set_query_hw_ip(&hw);
    let mut b = [0u8; 32];
    q.encode(&mut b).unwrap();
    let back = drm_amdgpu_info::decode(&b).unwrap();
    assert_eq!(back.anon0.query_hw_ip().r#type, AMDGPU_HW_IP_GFX);
}

#[test]
fn prime_export_flags_are_the_x86_64_open_flags() {
    // PRIME_HANDLE_TO_FD takes DRM_CLOEXEC | DRM_RDWR; linux-6.6/include/uapi/asm-generic/fcntl.h:22 and :63.
    assert_eq!(DRM_RDWR, 0o2);
    assert_eq!(DRM_CLOEXEC, 0o2000000);
}

#[test]
fn ioc_fields_are_bounded() {
    assert_eq!(ioc::checked_ioc(ioc::IOC_READ | ioc::IOC_WRITE, DRM_IOCTL_BASE, 0, 64), Some(DRM_IOCTL_VERSION));
    assert_eq!(ioc::checked_ioc(ioc::IOC_READ, DRM_IOCTL_BASE, 0, 0x4000), None);
    assert_eq!(ioc::checked_ioc(ioc::IOC_READ, 0x100, 0, 8), None);
    assert_eq!(ioc::checked_ioc(ioc::IOC_READ, DRM_IOCTL_BASE, 0x100, 8), None);
    assert_eq!(ioc::checked_ioc(4, DRM_IOCTL_BASE, 0, 8), None);
    const V: u32 = ioc::ioc(ioc::IOC_READ | ioc::IOC_WRITE, DRM_IOCTL_BASE, 0, 64);
    assert_eq!(V, DRM_IOCTL_VERSION);
}
