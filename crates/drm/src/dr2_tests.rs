//! DR-2 host tests: the dispatcher's copy rules and error order, the core ioctls, permissions per node kind,
//! handle tables, the registry's minors and the /sys view, all against the fake on byte buffers.

use crate::device::{self, Driver, DriverInfo, PciIdentity};
use crate::dispatch::{Caller, Desc};
use crate::fake::{FakeGpu, AMDGPU_COUNT, AMDGPU_TABLE};
use crate::file::{GemObject, MinorKind};
use crate::ioc::{self, IOC_NONE, IOC_READ, IOC_WRITE};
use crate::node::{open_device, OpenDrm};
use crate::uapi::*;
use crate::wire::Wire;
use crate::{core_ioctls, sysfs};
use linux::errno::*;
use std::sync::{Arc, Mutex};
use std::vec::Vec;
use std::{format, string::String, vec};
use vfs::synth::FlatKind;

const BASE: u64 = 0x10000;

/// A program's memory: one region at BASE; `ro` refuses writes (a copy-back fault).
struct Mem {
    b: Mutex<Vec<u8>>,
    ro: bool,
}

impl Mem {
    fn new(n: usize) -> Mem {
        Mem { b: Mutex::new(vec![0; n]), ro: false }
    }
    fn range(&self, addr: u64, n: usize) -> Option<core::ops::Range<usize>> {
        let s = usize::try_from(addr.checked_sub(BASE)?).ok()?;
        (s.checked_add(n)? <= self.b.lock().unwrap().len()).then_some(s..s + n)
    }
    fn set(&self, off: usize, d: &[u8]) {
        self.b.lock().unwrap()[off..off + d.len()].copy_from_slice(d);
    }
    fn get(&self, off: usize, n: usize) -> Vec<u8> {
        self.b.lock().unwrap()[off..off + n].to_vec()
    }
    fn u64(&self, off: usize) -> u64 {
        u64::from_le_bytes(self.get(off, 8).try_into().unwrap())
    }
    fn i32(&self, off: usize) -> i32 {
        i32::from_le_bytes(self.get(off, 4).try_into().unwrap())
    }
}

impl vfs::sys::UserMem for Mem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        match self.range(addr, buf.len()) {
            Some(r) => {
                buf.copy_from_slice(&self.b.lock().unwrap()[r]);
                true
            }
            None => false,
        }
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        match self.range(addr, data.len()) {
            Some(r) if !self.ro => {
                self.b.lock().unwrap()[r].copy_from_slice(data);
                true
            }
            _ => false,
        }
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

/// Tests that count on which minor the registry gives hold this (the registry is one for all test threads).
pub(crate) static MINORS: Mutex<()> = Mutex::new(());

const ME: Caller = Caller { pid: 42, sys_admin: false, sys_nice: false };

/// A registered fake and an open file of `kind`; the device leaves the registry when the guard drops.
struct Fixture {
    f: Arc<OpenDrm>,
}

impl Fixture {
    fn new(kind: MinorKind) -> Fixture {
        Fixture::with(Arc::new(FakeGpu::radeon_780m()), kind)
    }
    fn with(drv: Arc<dyn Driver>, kind: MinorKind) -> Fixture {
        let d = {
            let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
            device::register(drv).unwrap()
        };
        Fixture { f: open_device(d, kind, false).unwrap() }
    }
    fn io(&self, cmd: u32, arg: u64, m: &Mem) -> Result<i64, vfs::Errno> {
        self.f.ioctl_as(cmd, arg, m, ME)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
        device::unregister(self.f.device());
    }
}

fn with_size(cmd: u32, size: u32) -> u32 {
    ioc::ioc(ioc::dir(cmd), ioc::ty(cmd), ioc::nr(cmd), size)
}

#[test]
fn version_lengths_then_strings() {
    let fx = Fixture::new(MinorKind::Render);
    let m = Mem::new(4096);
    // First call: null pointers, zero lengths (libdrm's drmGetVersion).
    assert_eq!(fx.io(DRM_IOCTL_VERSION, BASE, &m), Ok(0));
    assert_eq!((m.i32(0), m.i32(4), m.i32(8)), (3, 54, 0));
    assert_eq!((m.u64(16), m.u64(32), m.u64(48)), (6, 8, 7));
    // Second call: buffers; the name buffer is short (3 bytes): "amd", the length still 6, no NUL written.
    let mut v = drm_version::ZERO;
    v.name_len = 3;
    v.name = BASE + 1024;
    v.date_len = 64;
    v.date = BASE + 1100;
    v.desc_len = 7;
    v.desc = BASE + 1200;
    let mut b = [0u8; 64];
    v.encode(&mut b).unwrap();
    m.set(0, &b);
    m.set(1024, &[0xee; 8]);
    assert_eq!(fx.io(DRM_IOCTL_VERSION, BASE, &m), Ok(0));
    assert_eq!(&m.get(1024, 4), b"amd\xee");
    assert_eq!(&m.get(1100, 9), b"20150101\0");
    assert_eq!(&m.get(1200, 7), b"AMD GPU");
    assert_eq!((m.u64(16), m.u64(32), m.u64(48)), (6, 8, 7));
}

#[test]
fn version_bad_buffer_is_efault_with_lengths_back() {
    let fx = Fixture::new(MinorKind::Render);
    let m = Mem::new(256);
    let mut v = drm_version::ZERO;
    v.name_len = 6;
    v.name = 0xdead_0000;
    let mut b = [0u8; 64];
    v.encode(&mut b).unwrap();
    m.set(0, &b);
    assert_eq!(fx.io(DRM_IOCTL_VERSION, BASE, &m), Err(EFAULT));
    assert_eq!(m.u64(16), 6);
    assert_eq!(m.i32(0), 3, "copied back after the error, as Linux");
}

#[test]
fn padding_comes_back_untouched() {
    let fx = Fixture::new(MinorKind::Render);
    let m = Mem::new(256);
    m.set(12, &[0x5a; 4]);
    assert_eq!(fx.io(DRM_IOCTL_VERSION, BASE, &m), Ok(0));
    assert_eq!(m.get(12, 4), vec![0x5a; 4]);
}

#[test]
fn caller_size_decides_the_copies() {
    let fx = Fixture::new(MinorKind::Render);
    // Smaller than the kernel's 64: only 32 bytes come back, the rest of the caller's memory stays.
    let m = Mem::new(256);
    m.set(32, &[0x77; 32]);
    assert_eq!(fx.io(with_size(DRM_IOCTL_VERSION, 32), BASE, &m), Ok(0));
    assert_eq!(m.i32(0), 3);
    assert_eq!(m.u64(16), 6);
    assert_eq!(m.get(32, 32), vec![0x77; 32]);
    // Larger: 80 bytes in and out, the tail unchanged.
    let m = Mem::new(256);
    m.set(64, &[0x33; 16]);
    m.set(80, &[0x44; 16]);
    assert_eq!(fx.io(with_size(DRM_IOCTL_VERSION, 80), BASE, &m), Ok(0));
    assert_eq!(m.get(64, 16), vec![0x33; 16]);
    assert_eq!(m.get(80, 16), vec![0x44; 16]);
    // Larger than the memory: the copy in faults.
    let m = Mem::new(64);
    assert_eq!(fx.io(with_size(DRM_IOCTL_VERSION, 80), BASE, &m), Err(EFAULT));
    // No direction bits: nothing is read or written, the handler sees zeros.
    let m = Mem::new(64);
    m.set(0, &[0x11; 64]);
    assert_eq!(fx.io(ioc::ioc(IOC_NONE, DRM_IOCTL_BASE, 0, 64), 0, &m), Ok(0));
    assert_eq!(m.get(0, 64), vec![0x11; 64]);
    // Write-only from the caller: read in, nothing back.
    let m = Mem::new(64);
    assert_eq!(fx.io(ioc::ioc(IOC_WRITE, DRM_IOCTL_BASE, 0, 64), BASE, &m), Ok(0));
    assert_eq!(m.i32(0), 0);
    // Read-only memory: the handler ran, the copy back faults.
    let m = Mem { b: Mutex::new(vec![0; 64]), ro: true };
    assert_eq!(fx.io(DRM_IOCTL_VERSION, BASE, &m), Err(EFAULT));
}

#[test]
fn numbers_without_entries() {
    let fx = Fixture::new(MinorKind::Render);
    let m = Mem::new(256);
    assert_eq!(fx.io(0x8004_4500, BASE, &m), Err(ENOTTY), "type 'E'");
    let hole = core_ioctls::TABLE.iter().position(|d| d.is_none()).unwrap() as u32;
    assert_eq!(fx.io(ioc::ioc(IOC_READ | IOC_WRITE, DRM_IOCTL_BASE, hole, 16), BASE, &m), Err(EINVAL));
    assert_eq!(fx.io(ioc::ioc(IOC_READ, DRM_IOCTL_BASE, 0xfe, 8), BASE, &m), Err(EINVAL));
    // Driver range: a hole (0x4a) and past the end (0x40 + 22).
    assert!(AMDGPU_TABLE[0x0a].is_none());
    assert_eq!(fx.io(ioc::ioc(IOC_WRITE, DRM_IOCTL_BASE, 0x4a, 8), BASE, &m), Err(EINVAL));
    assert_eq!(fx.io(ioc::ioc(IOC_WRITE, DRM_IOCTL_BASE, 0x40 + AMDGPU_COUNT as u32, 8), BASE, &m), Err(EINVAL));
    // libdrm 2.4.134 numbers Linux 6.6 lacks.
    for cmd in [DRM_IOCTL_AMDGPU_USERQ, DRM_IOCTL_AMDGPU_USERQ_SIGNAL, DRM_IOCTL_AMDGPU_USERQ_WAIT] {
        assert_eq!(fx.io(cmd, BASE, &m), Err(EINVAL), "{cmd:#x}");
    }
}

#[test]
fn render_node_refuses_what_linux_refuses() {
    let fx = Fixture::new(MinorKind::Render);
    let m = Mem::new(1024);
    for cmd in [DRM_IOCTL_GET_UNIQUE, DRM_IOCTL_GET_CLIENT, DRM_IOCTL_GET_MAGIC, DRM_IOCTL_SET_MASTER, DRM_IOCTL_SET_CLIENT_CAP, DRM_IOCTL_MODE_GETRESOURCES, DRM_IOCTL_GEM_FLINK, DRM_IOCTL_AMDGPU_SCHED] {
        assert_eq!(fx.io(cmd, BASE, &m), Err(EACCES), "{cmd:#x}");
    }
    // Allowed on render nodes but not done yet: EINVAL after the checks.
    assert_eq!(fx.io(DRM_IOCTL_AMDGPU_INFO, BASE, &m), Err(EINVAL));
    // DR-6: allowed and done: a zero size has no offset (ENOSPC), handle 0 is no object (ENOENT).
    assert_eq!(fx.io(DRM_IOCTL_AMDGPU_GEM_CREATE, BASE, &m), Err(ENOSPC));
    assert_eq!(fx.io(DRM_IOCTL_PRIME_HANDLE_TO_FD, BASE, &m), Err(ENOENT));
    // A fault in the copy comes before the permission check.
    assert_eq!(fx.io(DRM_IOCTL_GET_UNIQUE, 0, &m), Err(EFAULT));
}

#[test]
fn primary_node_unique_client_and_root_only() {
    let fx = Fixture::new(MinorKind::Primary);
    let m = Mem::new(1024);
    // GET_UNIQUE: too small a buffer gets only the length.
    m.set(0, &4u64.to_le_bytes());
    m.set(8, &(BASE + 512).to_le_bytes());
    assert_eq!(fx.io(DRM_IOCTL_GET_UNIQUE, BASE, &m), Ok(0));
    assert_eq!(m.u64(0), 12);
    assert_eq!(m.get(512, 4), vec![0; 4]);
    m.set(0, &64u64.to_le_bytes());
    assert_eq!(fx.io(DRM_IOCTL_GET_UNIQUE, BASE, &m), Ok(0));
    assert_eq!(&m.get(512, 13), b"0000:c7:00.0\0");
    // GET_CLIENT: index 0 is the caller, any other EINVAL.
    let m = Mem::new(1024);
    assert_eq!(fx.io(DRM_IOCTL_GET_CLIENT, BASE, &m), Ok(0));
    assert_eq!((m.i32(4), m.u64(8), m.u64(16)), (0, 42, 65534));
    m.set(0, &1i32.to_le_bytes());
    assert_eq!(fx.io(DRM_IOCTL_GET_CLIENT, BASE, &m), Err(EINVAL));
    // AUTH without authentication, ROOT_ONLY without CAP_SYS_ADMIN, MASTER without master.
    assert_eq!(fx.io(DRM_IOCTL_GEM_FLINK, BASE, &m), Err(EACCES));
    assert_eq!(fx.io(DRM_IOCTL_SET_UNIQUE, BASE, &m), Err(EACCES));
    assert_eq!(fx.io(DRM_IOCTL_MODE_SETCRTC, BASE, &m), Err(EACCES));
    // Flags 0 on a primary node: through to "not done yet".
    assert_eq!(fx.io(DRM_IOCTL_MODE_GETRESOURCES, BASE, &m), Err(EINVAL));
    assert_eq!(fx.io(DRM_IOCTL_GET_STATS, BASE, &m), Ok(0));
}

fn cap(fx: &Fixture, c: u32) -> Result<u64, vfs::Errno> {
    let m = Mem::new(16);
    m.set(0, &(c as u64).to_le_bytes());
    m.set(8, &u64::MAX.to_le_bytes());
    let r = fx.io(DRM_IOCTL_GET_CAP, BASE, &m);
    let v = m.u64(8);
    r.map(|_| v).map_err(|e| {
        assert_eq!(v, 0, "value 0 on error, cap {c:#x}");
        e
    })
}

#[test]
fn caps_as_amdgpu() {
    let fx = Fixture::new(MinorKind::Render);
    let want = [
        (DRM_CAP_DUMB_BUFFER, Ok(1)),
        (DRM_CAP_VBLANK_HIGH_CRTC, Ok(1)),
        (DRM_CAP_DUMB_PREFERRED_DEPTH, Ok(24)),
        (DRM_CAP_DUMB_PREFER_SHADOW, Ok(1)),
        (DRM_CAP_PRIME, Ok(3)),
        (DRM_CAP_TIMESTAMP_MONOTONIC, Ok(1)),
        (DRM_CAP_ASYNC_PAGE_FLIP, Ok(1)),
        (DRM_CAP_CURSOR_WIDTH, Ok(256)),
        (DRM_CAP_CURSOR_HEIGHT, Ok(256)),
        (DRM_CAP_ADDFB2_MODIFIERS, Ok(1)),
        (DRM_CAP_PAGE_FLIP_TARGET, Ok(0)),
        (DRM_CAP_CRTC_IN_VBLANK_EVENT, Ok(1)),
        (DRM_CAP_SYNCOBJ, Ok(1)),
        (DRM_CAP_SYNCOBJ_TIMELINE, Ok(1)),
        (DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP, Err(EINVAL)),
        (0, Err(EINVAL)),
        (0x7f, Err(EINVAL)),
    ];
    for (c, w) in want {
        assert_eq!(cap(&fx, c), w, "cap {c:#x}");
    }
    let m = Mem::new(16);
    m.set(0, &(1u64 << 32 | 5).to_le_bytes());
    assert_eq!(fx.io(DRM_IOCTL_GET_CAP, BASE, &m), Err(EINVAL), "the whole 64-bit capability counts");
}

struct RenderOnly(DriverInfo, PciIdentity);
impl Driver for RenderOnly {
    fn info(&self) -> &DriverInfo {
        &self.0
    }
    fn pci(&self) -> &PciIdentity {
        &self.1
    }
    fn ioctls(&self) -> &'static [Option<Desc>] {
        &[]
    }
}

#[test]
fn caps_of_a_driver_without_kms() {
    let mut info = crate::fake::AMDGPU_INFO;
    info.features.modeset = false;
    info.features.syncobj_timeline = false;
    let pci = FakeGpu::at(0xc8, 0).pci().clone();
    let fx = Fixture::with(Arc::new(RenderOnly(info, pci)), MinorKind::Render);
    assert_eq!(cap(&fx, DRM_CAP_PRIME), Ok(3));
    assert_eq!(cap(&fx, DRM_CAP_SYNCOBJ_TIMELINE), Ok(0));
    assert_eq!(cap(&fx, DRM_CAP_DUMB_BUFFER), Err(EOPNOTSUPP));
    assert_eq!(cap(&fx, 0x7f), Err(EOPNOTSUPP));
    // An empty driver table: every driver number is EINVAL.
    let m = Mem::new(64);
    assert_eq!(fx.io(DRM_IOCTL_AMDGPU_INFO, BASE, &m), Err(EINVAL));
}

struct Bo(u64);
impl GemObject for Bo {
    fn size(&self) -> u64 {
        self.0
    }
}

#[test]
fn handles_lowest_free_from_one() {
    let fx = Fixture::new(MinorKind::Render);
    let f = fx.f.drm_file();
    let obj: Arc<dyn GemObject> = Arc::new(Bo(4096));
    let hs: Vec<u32> = (0..5).map(|_| f.handle_create(obj.clone()).unwrap()).collect();
    assert_eq!(hs, vec![1, 2, 3, 4, 5]);
    f.handle_delete(2).unwrap();
    f.handle_delete(4).unwrap();
    assert_eq!(f.handle_delete(4), Err(EINVAL));
    assert_eq!(f.handle_create(obj.clone()), Ok(2));
    assert_eq!(f.handle_create(obj.clone()), Ok(4));
    assert_eq!(f.handle_create(obj.clone()), Ok(6));
    assert_eq!(f.handle_lookup(6).map(|o| o.size()), Some(4096));
    // GEM_CLOSE through the dispatcher: a known handle goes, an unknown one is EINVAL.
    let m = Mem::new(8);
    m.set(0, &3u32.to_le_bytes());
    assert_eq!(fx.io(DRM_IOCTL_GEM_CLOSE, BASE, &m), Ok(0));
    assert!(f.handle_lookup(3).is_none());
    assert_eq!(fx.io(DRM_IOCTL_GEM_CLOSE, BASE, &m), Err(EINVAL));
    assert_eq!(f.handle_count(), 5);
    // Every reference goes at the last close.
    assert_eq!(Arc::strong_count(&obj), 6);
    drop(fx);
    assert_eq!(Arc::strong_count(&obj), 1);
}

#[test]
fn handle_table_shrinks_and_reuses() {
    let mut t = crate::handles::Handles::new();
    for i in 1..=100u32 {
        assert_eq!(t.insert(i), Ok(i));
    }
    for i in (1..=100u32).rev() {
        assert_eq!(t.remove(i), Some(i));
    }
    assert!(t.is_empty());
    assert_eq!(t.insert(7), Ok(1));
    assert_eq!(t.drain().len(), 1);
    assert_eq!(t.insert(8), Ok(1));
}

#[test]
fn files_have_their_own_tables() {
    let fx = Fixture::new(MinorKind::Render);
    let other = open_device(fx.f.device().clone(), MinorKind::Render, false).unwrap();
    let obj: Arc<dyn GemObject> = Arc::new(Bo(1));
    assert_eq!(fx.f.drm_file().handle_create(obj.clone()), Ok(1));
    assert_eq!(other.drm_file().handle_create(obj), Ok(1));
    assert!(fx.f.drm_file().client_id() != other.drm_file().client_id());
}

#[test]
fn unplugged_device_answers_enodev() {
    let fx = Fixture::new(MinorKind::Render);
    let d = fx.f.device().clone();
    device::unregister(&d);
    let m = Mem::new(64);
    assert_eq!(fx.io(DRM_IOCTL_VERSION, BASE, &m), Err(ENODEV));
    assert!(open_device(d, MinorKind::Render, false).is_err());
}

#[test]
fn minors_lowest_free_and_the_view_lists_each() {
    let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
    let a = device::register(Arc::new(FakeGpu::at(0xd0, 0))).unwrap();
    let b = device::register(Arc::new(FakeGpu::at(0xd1, 0))).unwrap();
    assert!(a.render_minor() >= 128 && b.render_minor() > a.render_minor());
    let e = sysfs::entries_of(&[a.clone(), b.clone()]);
    for d in [&a, &b] {
        let m = d.render_minor();
        let mdir = format!("devices/pci0000:00/{}/drm/renderD{m}", d.unique());
        assert!(e.iter().any(|x| x.path == format!("dev/char/226:{m}")));
        assert!(e.iter().any(|x| x.path == mdir));
    }
    let mut paths: Vec<&String> = e.iter().map(|x| &x.path).collect();
    let n = paths.len();
    paths.dedup();
    assert_eq!(paths.len(), n, "no path twice");
    let am = a.render_minor();
    device::unregister(&a);
    let c = device::register(Arc::new(FakeGpu::at(0xd2, 0))).unwrap();
    assert_eq!(c.render_minor(), am, "the freed minor comes back");
    device::unregister(&b);
    device::unregister(&c);
}

/// Follows a relative link of the view the way realpath does (only `..` and names).
fn resolve(at: &str, target: &str) -> String {
    let mut parts: Vec<&str> = at.split('/').collect();
    parts.pop();
    for c in target.split('/') {
        match c {
            ".." => {
                parts.pop();
            }
            "" | "." => {}
            c => parts.push(c),
        }
    }
    parts.join("/")
}

fn find<'a>(e: &'a [vfs::synth::FlatEntry], p: &str) -> &'a FlatKind {
    &e.iter().find(|x| x.path == p).unwrap_or_else(|| panic!("no {p}")).kind
}

fn link_of(e: &[vfs::synth::FlatEntry], p: &str) -> String {
    match find(e, p) {
        FlatKind::Link(t) => resolve(p, t),
        _ => panic!("{p} is no link"),
    }
}

fn text_of(e: &[vfs::synth::FlatEntry], p: &str) -> String {
    match find(e, p) {
        FlatKind::Text(t) => t.clone(),
        _ => panic!("{p} is no text"),
    }
}

#[test]
fn sysfs_walk_as_libdrm() {
    // INT-W24: every registration takes MINORS, else minors_lowest_free_... sees a minor taken under it.
    let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
    let d = device::register(Arc::new(FakeGpu::radeon_780m())).unwrap();
    let m = d.render_minor();
    let e = sysfs::entries_of(&[d.clone()]);
    let node = link_of(&e, &format!("dev/char/226:{m}"));
    assert_eq!(node, format!("devices/pci0000:00/0000:c7:00.0/drm/renderD{m}"));
    assert_eq!(link_of(&e, &format!("class/drm/renderD{m}")), node);
    let dev = link_of(&e, &format!("{node}/device"));
    assert_eq!(dev, "devices/pci0000:00/0000:c7:00.0");
    assert!(matches!(find(&e, &format!("{dev}/drm")), FlatKind::Dir));
    assert_eq!(link_of(&e, &format!("{dev}/subsystem")), "bus/pci");
    assert_eq!(link_of(&e, &format!("{node}/subsystem")), "class/drm");
    let ue = text_of(&e, &format!("{dev}/uevent"));
    assert!(ue.starts_with("DRIVER=amdgpu\nPCI_CLASS=30000\nPCI_ID=1002:1900\nPCI_SUBSYS_ID=1002:0124\nPCI_SLOT_NAME=0000:c7:00.0\n"), "{ue}");
    assert_eq!(text_of(&e, &format!("{dev}/vendor")), "0x1002\n");
    assert_eq!(text_of(&e, &format!("{dev}/device")), "0x1900\n");
    assert_eq!(text_of(&e, &format!("{dev}/subsystem_vendor")), "0x1002\n");
    assert_eq!(text_of(&e, &format!("{dev}/subsystem_device")), "0x0124\n");
    assert_eq!(text_of(&e, &format!("{dev}/revision")), "0xd5\n");
    assert_eq!(text_of(&e, &format!("{node}/dev")), format!("226:{m}\n"));
    assert_eq!(text_of(&e, &format!("{node}/uevent")), format!("MAJOR=226\nMINOR={m}\nDEVNAME=dri/renderD{m}\nDEVTYPE=drm_minor\n"));
    assert_eq!(text_of(&e, "class/drm/version"), "drm 1.1.0 20060810\n");
    let c = sysfs::config_bytes(&d);
    assert_eq!(&c[0..4], &[0x02, 0x10, 0x00, 0x19]);
    assert_eq!(&c[8..12], &[0xd5, 0x00, 0x00, 0x03]);
    assert_eq!(&c[0x2c..0x30], &[0x02, 0x10, 0x24, 0x01]);
    // Every directory level is listed (the flat view needs each).
    for p in ["class", "class/drm", "dev", "dev/char", "devices", "devices/pci0000:00", dev.as_str()] {
        assert!(matches!(find(&e, p), FlatKind::Dir), "{p}");
    }
    assert!(sysfs::entries_of(&[]).is_empty());
    device::unregister(&d);
}

#[test]
fn tables_match_linux_66() {
    assert_eq!(core_ioctls::COUNT, 0xd0, "DRM_IOCTL_SYNCOBJ_EVENTFD (0xcf) is 6.6's last core number");
    assert_eq!(AMDGPU_COUNT, 0x16, "AMDGPU_SCHED (0x15) is 6.6's last amdgpu number");
    let v = core_ioctls::TABLE[0].unwrap();
    assert_eq!((v.cmd, v.name), (DRM_IOCTL_VERSION, "VERSION"));
    assert_eq!(core_ioctls::TABLE.iter().flatten().count(), 107);
    assert_eq!(AMDGPU_TABLE.iter().flatten().count(), 16);
    let va = AMDGPU_TABLE[0x08].unwrap();
    assert_eq!(ioc::size(va.cmd), 40, "GEM_VA keeps 6.6's size");
}

#[test]
fn fake_mode() {
    use device::{fake_wanted, Mode};
    assert!(fake_wanted(Mode::Fake) && !fake_wanted(Mode::Auto) && !fake_wanted(Mode::Off));
    assert_eq!(device::mode(), Mode::Auto, "auto without CONFIG");
}

/// Random numbers, sizes and bytes never panic and answer only errno values Linux would.
#[test]
fn fuzz_never_panics() {
    let fx = Fixture::new(MinorKind::Render);
    let pfx = Fixture::new(MinorKind::Primary);
    let mut s: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let m = Mem::new(1 << 15);
    for _ in 0..20000 {
        let r = next();
        let cmd = ioc::ioc((r & 3) as u32, DRM_IOCTL_BASE, (r >> 8) as u32 & 0xff, (r >> 16) as u32 & 0x3fff);
        let fill = next().to_le_bytes();
        let off = (next() % 4096) as usize;
        m.set(off, &fill);
        let arg = if r >> 40 & 7 == 0 { 0 } else { BASE + (next() % 2048) };
        // DR-3: the syncobj waits (and TRANSFER's WAIT_FOR_SUBMIT) block until a random absolute time; dr3_tests.
        if [0xc3, 0xca, 0xcc].contains(&ioc::nr(cmd)) {
            continue;
        }
        for f in [&fx, &pfx] {
            if let Err(e) = f.io(cmd, arg, &m) {
                assert!([EINVAL, EFAULT, EACCES, ENOTTY, EOPNOTSUPP, ENOMEM, ENOENT, EMFILE, EBADF, ENOSPC, EPERM, ENODEV].contains(&e), "{cmd:#x}: {e}");
            }
        }
    }
}

#[test]
fn unplug_waits_for_a_call_inside_the_device() {
    let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
    let d = device::register(Arc::new(FakeGpu::at(0xd3, 0))).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let d2 = d.clone();
    let t = std::thread::spawn(move || {
        let g = d2.enter().expect("not unplugged yet");
        tx.send(()).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        drop(g);
    });
    rx.recv().unwrap();
    let t0 = std::time::Instant::now();
    device::unplug(&d);
    assert!(t0.elapsed() >= std::time::Duration::from_millis(30), "unplug returned while a call was inside");
    assert_eq!(d.inflight(), 0);
    assert!(d.enter().is_none(), "no call enters after unplug");
    t.join().unwrap();
    device::release_minor(&d);
}

#[test]
fn the_minor_stays_taken_until_released() {
    let _g = MINORS.lock().unwrap_or_else(|e| e.into_inner());
    let a = device::register(Arc::new(FakeGpu::at(0xd4, 0))).unwrap();
    device::unplug(&a);
    let b = device::register(Arc::new(FakeGpu::at(0xd5, 0))).unwrap();
    assert!(b.render_minor() != a.render_minor(), "the node of an unplugged device may still exist");
    device::release_minor(&a);
    let c = device::register(Arc::new(FakeGpu::at(0xd6, 0))).unwrap();
    assert_eq!(c.render_minor(), a.render_minor());
    device::unregister(&b);
    device::unregister(&c);
}
