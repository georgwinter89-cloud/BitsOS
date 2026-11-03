//! DR-2 in-kernel tests (all Safe and rerunnable): the node, ioctls through the open file, the /sys walk through
//! the real VFS (links, merged directories), a second device at run time, and the ABI program `/bin/abi-drm`.
//! They use the boot device (`drm=fake`); without one, under a hypervisor each test adds a fake at renderD128 for
//! its own run and removes it again (node and /sys entries gone afterwards); on hardware without a device they skip.

use crate::device;
use crate::dispatch::Caller;
use crate::fake::FakeGpu;
use crate::uapi::*;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use ksync::SpinLock;
use ktest::{kassert, ktest, TestResult};
use linux::errno::*;

pub(crate) const BASE: u64 = 0x1000_0000;
pub(crate) const KERNEL: Caller = Caller { pid: 0, sys_admin: true, sys_nice: true };
const O_RDWR: u64 = 0o2;
const O_CLOEXEC: u64 = 0o2000000;

/// Memory for the ioctls a test makes: one buffer at BASE.
pub(crate) struct KMem(SpinLock<Vec<u8>>);

impl KMem {
    pub(crate) fn new(n: usize) -> KMem {
        KMem(SpinLock::named("drm.ktest.mem", vec![0; n]))
    }
    pub(crate) fn at(&self, off: usize, n: usize) -> Vec<u8> {
        self.0.lock()[off..off + n].to_vec()
    }
    pub(crate) fn u64(&self, off: usize) -> u64 {
        u64::from_le_bytes(self.at(off, 8).try_into().unwrap_or([0; 8]))
    }
    pub(crate) fn set(&self, off: usize, d: &[u8]) {
        self.0.lock()[off..off + d.len()].copy_from_slice(d);
    }
    fn span(&self, addr: u64, n: usize) -> Option<core::ops::Range<usize>> {
        let s = usize::try_from(addr.checked_sub(BASE)?).ok()?;
        (s.checked_add(n)? <= self.0.lock().len()).then_some(s..s + n)
    }
}

impl vfs::sys::UserMem for KMem {
    fn read(&self, addr: u64, buf: &mut [u8]) -> bool {
        let Some(r) = self.span(addr, buf.len()) else { return false };
        buf.copy_from_slice(&self.0.lock()[r]);
        true
    }
    fn write(&self, addr: u64, data: &[u8]) -> bool {
        let Some(r) = self.span(addr, data.len()) else { return false };
        self.0.lock()[r].copy_from_slice(data);
        true
    }
    fn read_cstr(&self, _addr: u64, _max: usize) -> Result<Vec<u8>, vfs::Errno> {
        Err(EFAULT)
    }
}

/// The device a test uses and whether the test added it (then `done` removes it).
pub(crate) struct Dev {
    pub(crate) dev: Arc<device::Device>,
    added: bool,
}

impl Dev {
    pub(crate) fn get() -> Result<Option<Dev>, String> {
        if let Some(dev) = device::by_render_minor(device::RENDER_BASE) {
            return Ok(Some(Dev { dev, added: false }));
        }
        if !hal::serial::under_hypervisor() || !device::devices().is_empty() {
            return Ok(None);
        }
        let dev = crate::add_device(Arc::new(FakeGpu::radeon_780m())).map_err(|e| format!("add the fake: errno {e}"))?;
        Ok(Some(Dev { dev, added: true }))
    }
    fn node(&self) -> String {
        format!("/dev/dri/renderD{}", self.dev.render_minor())
    }
    pub(crate) fn done(self, r: TestResult) -> TestResult {
        if self.added {
            crate::remove_device(&self.dev);
        }
        r
    }
}

macro_rules! with_dev {
    ($d:ident, $body:block) => {{
        let Some($d) = Dev::get()? else { return ktest::skip("no DRM device on hardware (drm=fake gives one)") };
        let r = (|| -> TestResult { $body })();
        $d.done(r)
    }};
}
pub(crate) use with_dev;

/// Opens a node as a program does (libdrm's O_RDWR | O_CLOEXEC).
fn open(path: &str) -> Result<Arc<vfs::fd::OpenFile>, String> {
    vfs::root_ns().open_flags(path.as_bytes(), O_RDWR | O_CLOEXEC, 0).map_err(|e| format!("open {path}: errno {e}"))
}

fn node() -> TestResult {
    with_dev!(d, {
        let path = d.node();
        let dn = vfs::root_ns().lookup(&path).map_err(|e| format!("lookup {path}: errno {e}"))?;
        let f = vfs::node::file_of(&dn);
        kassert!(f.mode() == linux::S_IFCHR | 0o666, "mode {:o}", f.mode());
        kassert!(f.rdev() == vfs::abi::encode_dev(226, d.dev.render_minor()), "rdev {:#x}", f.rdev());
        kassert!(vfs::root_ns().lookup("/dev/dri").map(|d| d.is_dir()).unwrap_or(false), "/dev/dri is no directory");
        Ok(())
    })
}
ktest!("dr.node.made", Safe, node);

fn ioctls_through_the_file() -> TestResult {
    with_dev!(d, {
        let of = open(&d.node())?;
        let m = KMem::new(4096);
        let io = |cmd: u32, arg: u64| of.file.ioctl_user(cmd as u64, arg, &m).unwrap_or(Err(ENOTTY));
        kassert!(io(DRM_IOCTL_VERSION, BASE) == Ok(0));
        kassert!(m.u64(16) == 6 && m.u64(32) == 8 && m.u64(48) == 7, "lengths {} {} {}", m.u64(16), m.u64(32), m.u64(48));
        m.set(16, &16u64.to_le_bytes());
        m.set(24, &(BASE + 1024).to_le_bytes());
        kassert!(io(DRM_IOCTL_VERSION, BASE) == Ok(0));
        kassert!(m.at(1024, 6) == b"amdgpu", "name {:?}", m.at(1024, 6));
        let cap = |c: u32| {
            m.set(2048, &(c as u64).to_le_bytes());
            io(DRM_IOCTL_GET_CAP, BASE + 2048).map(|_| m.u64(2056))
        };
        kassert!(cap(DRM_CAP_PRIME) == Ok(3));
        kassert!(cap(DRM_CAP_SYNCOBJ_TIMELINE) == Ok(1));
        kassert!(cap(0x7f) == Err(EINVAL));
        kassert!(io(DRM_IOCTL_GET_UNIQUE, BASE) == Err(EACCES), "render nodes refuse GET_UNIQUE");
        kassert!(io(DRM_IOCTL_AMDGPU_INFO, BASE) == Err(EINVAL), "INFO is DR-3's");
        kassert!(io(0x8004_4500, BASE) == Err(ENOTTY));
        // Two opens are two files (client ids differ in fdinfo).
        let of2 = open(&d.node())?;
        let (mut a, mut b) = (String::new(), String::new());
        of.file.show_fdinfo(&mut a);
        of2.file.show_fdinfo(&mut b);
        kassert!(a.starts_with("drm-driver:\tamdgpu\ndrm-client-id:\t") && a.contains("drm-pdev:\t0000:c7:00.0\n"), "{a}");
        kassert!(a != b, "one client id for two opens: {a}");
        Ok(())
    })
}
ktest!("dr.node.ioctl", Safe, ioctls_through_the_file);

fn read_text(path: &str) -> Result<String, String> {
    let of = vfs::root_ns().open_flags(path.as_bytes(), 0, 0).map_err(|e| format!("open {path}: errno {e}"))?;
    let mut b = vec![0u8; 4096];
    let n = of.file.read_at(0, &mut b).map_err(|e| format!("read {path}: errno {e}"))?;
    b.truncate(n);
    String::from_utf8(b).map_err(|_| format!("{path}: not UTF-8"))
}

fn same(a: &str, b: &str) -> Result<bool, String> {
    let x = vfs::root_ns().lookup(a).map_err(|e| format!("{a}: errno {e}"))?;
    let y = vfs::root_ns().lookup(b).map_err(|e| format!("{b}: errno {e}"))?;
    Ok(x.inode.meta.ino == y.inode.meta.ino)
}

fn sysfs_walk() -> TestResult {
    with_dev!(d, { walk(d.dev.render_minor()) })
}

fn walk(m: u32) -> TestResult {
    let n = format!("/sys/dev/char/226:{m}");
    let n = n.as_str();
    kassert!(vfs::root_ns().lookup(&format!("{n}/device/drm")).map(|d| d.is_dir()).unwrap_or(false), "{n}/device/drm");
    kassert!(same(&format!("{n}/device/subsystem"), "/sys/bus/pci")?, "subsystem is not /sys/bus/pci");
    kassert!(same(&format!("{n}/device"), "/sys/devices/pci0000:00/0000:c7:00.0")?, "device link");
    kassert!(same(&format!("/sys/class/drm/renderD{m}"), n)?, "class/drm and dev/char differ");
    kassert!(same(&format!("{n}/subsystem"), "/sys/class/drm")?, "minor subsystem");
    let ue = read_text(&format!("{n}/device/uevent"))?;
    kassert!(ue.contains("\nPCI_SLOT_NAME=0000:c7:00.0\n"), "uevent: {ue}");
    kassert!(read_text(&format!("{n}/device/vendor"))? == "0x1002\n");
    kassert!(read_text(&format!("{n}/device/device"))? == "0x1900\n");
    kassert!(read_text(&format!("{n}/dev"))? == format!("226:{m}\n"));
    kassert!(read_text("/sys/class/drm/version")? == "drm 1.1.0 20060810\n");
    // PCI's own tree is still there next to the DRM entries (the directories merge).
    kassert!(vfs::root_ns().lookup("/sys/bus/pci/devices").is_ok(), "PCI's view is gone");
    Ok(())
}
ktest!("dr.sysfs.walk", Safe, sysfs_walk);

fn second_device() -> TestResult {
    with_dev!(first, { second(&first) })
}

fn second(_first: &Dev) -> TestResult {
    let d = crate::add_device(Arc::new(FakeGpu::at(0xc8, 0))).map_err(|e| format!("add: errno {e}"))?;
    let m = d.render_minor();
    let path = format!("/dev/dri/renderD{m}");
    let r = (|| -> TestResult {
        kassert!(m > device::RENDER_BASE, "minor {m}");
        let f = vfs::node::file_of(&vfs::root_ns().lookup(&path).map_err(|e| format!("{path}: errno {e}"))?);
        kassert!(f.rdev() == vfs::abi::encode_dev(226, m));
        let of = open(&path)?;
        let km = KMem::new(64);
        kassert!(of.file.ioctl_user(DRM_IOCTL_VERSION as u64, BASE, &km) == Some(Ok(0)));
        kassert!(read_text(&format!("/sys/dev/char/226:{m}/device/uevent"))?.contains("PCI_SLOT_NAME=0000:c8:00.0"));
        crate::remove_device(&d);
        kassert!(of.file.ioctl_user(DRM_IOCTL_VERSION as u64, BASE, &km) == Some(Err(ENODEV)), "open file after unplug");
        kassert!(vfs::root_ns().lookup(&path).err() == Some(ENOENT), "{path} stays after unplug");
        kassert!(vfs::root_ns().lookup(&format!("/sys/dev/char/226:{m}")).is_err(), "/sys entry stays after unplug");
        Ok(())
    })();
    crate::remove_device(&d);
    r
}
ktest!("dr.device.second", Safe, second_device);

struct KBo;
impl crate::file::GemObject for KBo {
    fn size(&self) -> u64 {
        4096
    }
}

/// An open file with a handle across `remove_device`: ioctls answer ENODEV, the handle lives until the last
/// close and goes with it (Linux: drm_release frees the handles of an unplugged device too).
fn unplug_open() -> TestResult {
    with_dev!(_first, { unplug_with_open_file() })
}

fn unplug_with_open_file() -> TestResult {
    let d = crate::add_device(Arc::new(FakeGpu::at(0xc9, 0))).map_err(|e| format!("add: errno {e}"))?;
    let m = d.render_minor();
    let r = (|| -> TestResult {
        let f = crate::node::open(m).map_err(|e| format!("open minor {m}: errno {e}"))?;
        let bo: Arc<dyn crate::file::GemObject> = Arc::new(KBo);
        let h = f.drm_file().handle_create(bo.clone()).map_err(|e| format!("handle: errno {e}"))?;
        kassert!(h == 1, "first handle {h}");
        let km = KMem::new(64);
        crate::remove_device(&d);
        kassert!(f.ioctl_as(DRM_IOCTL_VERSION, BASE, &km, KERNEL) == Err(ENODEV), "VERSION after unplug");
        km.set(0, &h.to_le_bytes());
        kassert!(f.ioctl_as(DRM_IOCTL_GEM_CLOSE, BASE, &km, KERNEL) == Err(ENODEV), "GEM_CLOSE after unplug");
        kassert!(crate::node::open(m).err() == Some(ENODEV), "open of a removed minor");
        kassert!(d.inflight() == 0, "calls left inside: {}", d.inflight());
        kassert!(Arc::strong_count(&bo) == 2, "handle freed before the last close");
        drop(f);
        kassert!(Arc::strong_count(&bo) == 1, "handle not freed at the last close");
        Ok(())
    })();
    crate::remove_device(&d);
    r
}
ktest!("dr.device.unplug_open", Safe, unplug_open);

fn direct_dispatch() -> TestResult {
    with_dev!(dv, {
        let d = crate::node::open(dv.dev.render_minor()).map_err(|e| format!("errno {e}"))?;
        let m = KMem::new(128);
        m.set(64, &[0x77; 64]);
        let cmd = crate::ioc::ioc(3, DRM_IOCTL_BASE, 0, 128);
        kassert!(d.ioctl_as(cmd, BASE, &m, KERNEL) == Ok(0));
        kassert!(m.at(64, 64) == vec![0x77; 64], "the caller's tail changed");
        Ok(())
    })
}

/// After a test's own fake is gone, nothing of it is left (Linux without a GPU: no /dev/dri/renderD*, no class/drm).
fn removed_cleanly() -> TestResult {
    if !device::devices().is_empty() || !hal::serial::under_hypervisor() {
        return ktest::skip("a DRM device is registered (drm=fake) or no hypervisor");
    }
    let d = crate::add_device(Arc::new(FakeGpu::radeon_780m())).map_err(|e| format!("errno {e}"))?;
    crate::remove_device(&d);
    kassert!(vfs::root_ns().lookup("/dev/dri/renderD128").err() == Some(ENOENT), "node left");
    kassert!(vfs::root_ns().lookup("/sys/class/drm").is_err(), "/sys/class/drm left");
    kassert!(vfs::root_ns().lookup("/sys/dev/char/226:128").is_err(), "/sys/dev/char/226:128 left");
    kassert!(vfs::root_ns().lookup("/sys/bus/pci/devices").is_ok(), "PCI's view is gone");
    Ok(())
}
ktest!("dr.device.removed", Safe, removed_cleanly);
ktest!("dr.dispatch.sizes", Safe, direct_dispatch);

const PROG: &str = "/bin/abi-drm";

/// The ABI program on V2 itself: every check line says yes, the node is found.
fn program() -> TestResult {
    if vfs::root_ns().lookup(PROG).is_err() {
        return ktest::skip("the ABI-diff programs are not in this image (no musl sysroot when it was built)");
    }
    with_dev!(_d, { run_program() })
}

fn run_program() -> TestResult {
    let out = proc::stdio::Capture::new(1 << 16);
    let env = ["HOME=/", "PATH=/bin", "ABI_TEST=1"];
    let argv = ["abi-drm"];
    let mut o = proc::SpawnOptions::new(&argv, &env, policy::ProgramId(1));
    o.stdout = Some(out.clone() as Arc<dyn vfs::File>);
    o.stderr = Some(out.clone() as Arc<dyn vfs::File>);
    let p = proc::spawn_with(PROG, &o).map_err(|e| format!("spawn {PROG}: {e}"))?;
    let ended = p.wait(Some(30_000_000_000)).is_some();
    if !ended {
        p.kill(9);
        p.wait(Some(5_000_000_000));
    }
    let text = String::from_utf8_lossy(&out.contents()).into_owned();
    kassert!(ended && p.status() == Some(proc::ExitStatus::Exited(0)), "{PROG} ended {:?}: {text}", p.status());
    kassert!(text.starts_with("amdgpu render node: found\n"), "{text}");
    let no: Vec<&str> = text.lines().filter(|l| l.ends_with(": no")).collect();
    kassert!(no.is_empty(), "{PROG}: {no:?}");
    Ok(())
}
ktest!("dr.program", Safe, program);
